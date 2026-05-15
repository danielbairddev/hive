use crate::{
    config::BotConfig,
    hivegame_bot_api::HiveGameApi,
    turn_tracker::{TurnTracker, TurnTracking},
    BotGameTurn,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tracing::{debug, error, info};

// Constants for token and login management
const LOGIN_RETRY_INTERVAL_SECS: u64 = 10;
const TOKEN_REFRESH_INTERVAL_SECS: u64 = 3600; // Refresh token every 60 minutes

async fn auth(api: &HiveGameApi, bot: &BotConfig) -> String {
    // Authenticate to get token with retry logic
    loop {
        match api.auth(&bot.email, &bot.password).await {
            Ok(token) => {
                info!("Authentication successful for bot: {}", bot.name);
                return token;
            }
            Err(e) => {
                error!(
                    "Authentication failed for bot {}: {}, retrying in {} seconds",
                    bot.name, e, LOGIN_RETRY_INTERVAL_SECS
                );
                tokio::time::sleep(Duration::from_secs(LOGIN_RETRY_INTERVAL_SECS)).await;
            }
        }
    }
}

pub async fn producer_task(
    sender: mpsc::Sender<BotGameTurn>,
    turn_tracker: TurnTracker,
    api: Arc<HiveGameApi>,
    bot: BotConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Producer task started for bot: {}", bot.name);

    let mut token = String::new();
    let mut token_time = Instant::now();

    loop {
        // Check if token needs to be refreshed (after refresh interval) or is empty
        if token.is_empty()
            || token_time.elapsed() > Duration::from_secs(TOKEN_REFRESH_INTERVAL_SECS)
        {
            token = auth(&api, &bot).await;
            token_time = Instant::now();
        }
        // Get challenges for this bot
        match api.challenges(&token).await {
            Ok(challenge_ids) => {
                if !challenge_ids.is_empty() {
                    info!(
                        "Bot {} has {} pending challenges: {:?}",
                        bot.name,
                        challenge_ids.len(),
                        challenge_ids
                    );

                    // Accept each challenge
                    for challenge_id in challenge_ids {
                        match api.accept_challenge(&challenge_id, &token).await {
                            Ok(_) => {
                                info!(
                                    "Bot {} successfully accepted challenge {}",
                                    bot.name, challenge_id
                                );
                            }
                            Err(e) => {
                                error!(
                                    "Bot {} failed to accept challenge {}: {}",
                                    bot.name, challenge_id, e
                                );
                            }
                        }
                    }
                } else {
                    debug!("No challenges found for bot {}", bot.name);
                }
            }
            Err(e) => error!("Failed to fetch challenges for bot {}: {}", bot.name, e),
        }

        match api.get_games(&token).await {
            Ok(game_strings) => {
                debug!(
                    "Retrieved {} games for bot {}",
                    game_strings.len(),
                    bot.name
                );
                for game in game_strings {
                    let hash = game.hash();

                    if turn_tracker.tracked(hash).await {
                        debug!("Game {} already tracked for bot {}", hash, bot.name);
                        continue;
                    }

                    if game.has_pending_takeback_request() {
                        let game_id = game.nanoid.as_deref().unwrap_or(&game.game_id);
                        let accept = !game.rated;
                        info!(
                            "Bot {} responding to takeback request in game {} (rated={}, accept={})",
                            bot.name, game_id, game.rated, accept
                        );
                        turn_tracker.processing(hash).await;
                        match api.respond_to_takeback(game_id, accept, &token).await {
                            Ok(_) => info!(
                                "Bot {} {} takeback in game {}",
                                bot.name,
                                if accept { "accepted" } else { "rejected" },
                                game_id
                            ),
                            Err(e) => error!(
                                "Bot {} failed to respond to takeback in game {}: {}",
                                bot.name, game_id, e
                            ),
                        }
                        turn_tracker.processed(hash).await;
                        continue;
                    }

                    let turn = BotGameTurn {
                        game,
                        hash,
                        bot: bot.clone(),
                        token: token.clone(),
                    };

                    turn_tracker.processing(hash).await;
                    debug!("Processing game {} for bot {}", hash, bot.name);

                    if sender.send(turn).await.is_err() {
                        error!("Failed to send turn to queue for bot {}", bot.name);
                        continue;
                    }
                }
            }
            Err(e) => error!("Failed to fetch games for bot {}: {}", bot.name, e),
        }

        debug!("Starting new cycle for bot {}", bot.name);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub async fn consumer_task(
    receiver: Arc<Mutex<mpsc::Receiver<BotGameTurn>>>,
    semaphore: Arc<Semaphore>,
    active_processes: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    turn_tracker: TurnTracker,
    api: Arc<HiveGameApi>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Consumer task started");

    loop {
        let mut rx = receiver.lock().await;
        if let Some(turn) = rx.recv().await {
            drop(rx);
            debug!("Received turn for bot {}", turn.bot.name);

            let api_clone = api.clone();
            let handle = tokio::spawn(process_turn(
                turn,
                semaphore.clone(),
                turn_tracker.clone(),
                api_clone,
            ));

            active_processes.lock().await.push(handle);
            cleanup_processes(active_processes.clone()).await;
        }
    }
}

async fn process_turn(
    turn: BotGameTurn,
    semaphore: Arc<Semaphore>,
    turn_tracker: TurnTracker,
    api: Arc<HiveGameApi>,
) {
    let _permit = match semaphore.acquire().await {
        Ok(permit) => permit,
        Err(e) => {
            error!(
                "Failed to acquire semaphore for bot {}: {}",
                turn.bot.name, e
            );
            turn_tracker.processed(turn.hash).await;
            return;
        }
    };

    debug!(
        "Processing turn for bot {} with hash {}",
        turn.bot.name, turn.hash
    );

    let child = match crate::ai::spawn_process(&turn.bot.ai_command, &turn.bot.name) {
        Ok(child) => child,
        Err(e) => {
            error!(
                "Failed to spawn AI process for bot {}: {}",
                turn.bot.name, e
            );
            turn_tracker.processed(turn.hash).await;
            return;
        }
    };

    // Convert game to string using the HiveGame method
    let game_string = turn.game.game_string();

    let game_identifier = match &turn.game.nanoid {
        Some(id) => id.clone(),
        None => turn.game.game_id.clone(),
    };

    match crate::ai::run_commands(child, &game_string, &turn.bot.bestmove_command_args).await {
        Ok(bestmove) => {
            info!("Bot '{}' bestmove: '{}'", turn.bot.name, bestmove);

            // Re-fetch game state before submitting: a takeback may have been requested
            // while the AI was computing. If we submit a move with a pending takeback the
            // server silently auto-rejects it, which is the wrong behaviour.
            let takeback_pending = match api.get_games(&turn.token).await {
                Ok(current_games) => current_games
                    .iter()
                    .find(|g| {
                        g.nanoid.as_deref() == Some(game_identifier.as_str())
                            || g.game_id == game_identifier
                    })
                    .is_some_and(|g| g.has_pending_takeback_request()),
                Err(e) => {
                    error!(
                        "Bot '{}' could not re-fetch game {} before move, proceeding anyway: {}",
                        turn.bot.name, game_identifier, e
                    );
                    false
                }
            };

            if takeback_pending {
                let accept = !turn.game.rated;
                info!(
                    "Bot '{}' detected takeback request in game {} during AI computation (rated={}, accept={})",
                    turn.bot.name, game_identifier, turn.game.rated, accept
                );
                match api.respond_to_takeback(&game_identifier, accept, &turn.token).await {
                    Ok(_) => info!(
                        "Bot '{}' {} takeback in game {} (post-computation)",
                        turn.bot.name,
                        if accept { "accepted" } else { "rejected" },
                        game_identifier
                    ),
                    Err(e) => error!(
                        "Bot '{}' failed to respond to takeback in game {}: {}",
                        turn.bot.name, game_identifier, e
                    ),
                }
            } else {
                match api
                    .play_move(&game_identifier, &bestmove, &turn.token)
                    .await
                {
                    Ok(_) => {
                        info!(
                            "Move '{}' sent successfully for game {}",
                            bestmove, game_identifier
                        );
                    }
                    Err(e) => {
                        error!(
                            "Failed to send move for bot '{}' on game {}: '{}'",
                            turn.bot.name, game_identifier, e
                        );
                    }
                }
            }
        }
        Err(e) => {
            error!(
                "Error running AI commands for bot '{}' on game {}: '{}'",
                turn.bot.name, game_identifier, e
            );
        }
    }

    turn_tracker.processed(turn.hash).await;
    debug!(
        "Turn processed for bot {} with hash {}",
        turn.bot.name, turn.hash
    );
}

pub async fn cleanup_processes(active_processes: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>) {
    let mut processes = active_processes.lock().await;
    let initial_count = processes.len();
    processes.retain(|handle| !handle.is_finished());
    let removed = initial_count - processes.len();
    if removed > 0 {
        debug!("Cleaned up {} finished processes", removed);
    }
}
