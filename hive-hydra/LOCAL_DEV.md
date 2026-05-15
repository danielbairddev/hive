# Hive-Hydra Local Development Setup

This document explains how to run the full hivegame stack locally — web app,
database, nokamute AI engine, and hive-hydra bot runner — so you can test bot
gameplay and the takeback feature end-to-end.

## Prerequisites

- [Docker Desktop](https://www.docker.com/products/docker-desktop/) (runs the
  web app and PostgreSQL)
- Rust nightly (runs hive-hydra and builds nokamute)
- The [nokamute](https://github.com/edre/nokamute) source tree checked out
  alongside this repo (e.g. `../nokamute` relative to the hive repo root)

---

## 1. Start the web app and database

From the repo root (any worktree that contains `docker-compose.yml`):

```sh
docker compose up postgres app
```

This starts:
- **PostgreSQL** on `localhost:5433`
- **The web app** on `http://localhost:3000` (built with `cargo leptos watch`,
  hot-reloaded on file changes)

The first startup takes several minutes while the Docker image is built and the
Rust code is compiled inside the container.

---

## 2. Create bot accounts (first time only)

The three nokamute bot accounts need to exist in the database with `bot = true`
and the correct argon2id password hashes. Run these once against the running
postgres container:

```sh
# Enable pgcrypto (needed for gen_salt if using bcrypt fallback)
docker exec hive-postgres-1 psql -U hive-dev -d hive-local \
  -c "CREATE EXTENSION IF NOT EXISTS pgcrypto;"
```

Then for each bot, build a one-shot hasher in the app container and insert:

```sh
# Build the password hasher tool (only needed once; reuse across bots)
docker exec hive-app-1 bash -c '
mkdir -p /app/tmp_hash/src
cat > /app/tmp_hash/Cargo.toml << TOML
[package]
name = "tmp-hash"
version = "0.1.0"
edition = "2021"
[dependencies]
argon2 = { version = "0.5", features = ["std"] }
password-hash = { version = "0.5", features = ["rand_core", "std"] }
[workspace]
TOML
cat > /app/tmp_hash/src/main.rs << RUST
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
    Argon2,
};
fn main() {
    let pw = std::env::args().nth(1).unwrap_or_default();
    let salt = SaltString::generate(&mut OsRng);
    println!("{}", Argon2::default().hash_password(pw.as_bytes(), &salt).unwrap());
}
RUST
'

for BOT in nokamute-easy nokamute-medium nokamute-hard; do
  HASH=$(docker exec hive-app-1 bash -c \
    "cd /app/tmp_hash && /usr/local/cargo/bin/cargo run -q -- $BOT 2>/dev/null")
  docker exec hive-postgres-1 psql -U hive-dev -d hive-local -c "
    INSERT INTO users
      (id, email, username, normalized_username, password, created_at, updated_at, bot)
    SELECT gen_random_uuid(),
           '${BOT}@example.com', '${BOT}', '${BOT}',
           '${HASH}', NOW(), NOW(), true
    WHERE NOT EXISTS (SELECT 1 FROM users WHERE username = '${BOT}');
  "
done
```

Verify:
```sh
docker exec hive-postgres-1 psql -U hive-dev -d hive-local \
  -c "SELECT username, bot FROM users WHERE bot = true ORDER BY username;"
```

Expected output:
```
   username     | bot
----------------+-----
 nokamute-easy  | t
 nokamute-hard  | t
 nokamute-medium| t
```

---

## 3. Build nokamute

From the nokamute source directory:

```sh
cd /path/to/nokamute
cargo build --release
```

The binary will be at `target/release/nokamute`.

---

## 4. Configure hive-hydra

Edit `hive-hydra/hive-hydra.yaml` and ensure the `ai_command` lines point at
the full path to the nokamute binary you just built:

```yaml
base_url: "http://localhost:3000"

bots:
  - name: nokamute-easy
    ai_command: /path/to/nokamute uhp --num-threads=1
    bestmove_command_args: depth 2
    email: nokamute-easy@example.com
    password: nokamute-easy

  - name: nokamute-medium
    ai_command: /path/to/nokamute uhp --num-threads=1
    bestmove_command_args: depth 4
    email: nokamute-medium@example.com
    password: nokamute-medium

  - name: nokamute-hard
    ai_command: /path/to/nokamute uhp --num-threads=2
    bestmove_command_args: depth 7
    email: nokamute-hard@example.com
    password: nokamute-hard
```

> **Passwords** — avoid putting plain-text passwords in YAML. Use environment
> variables instead (hive-hydra reads `HIVE_HYDRA_BOT_{NAME}_PASSWORD`):
> ```sh
> export HIVE_HYDRA_BOT_NOKAMUTE_EASY_PASSWORD=nokamute-easy
> export HIVE_HYDRA_BOT_NOKAMUTE_MEDIUM_PASSWORD=nokamute-medium
> export HIVE_HYDRA_BOT_NOKAMUTE_HARD_PASSWORD=nokamute-hard
> ```
> These are already set in the repo's `.env` file.

---

## 5. Run hive-hydra

Build hive-hydra (from the repo root):

```sh
cargo build --release -p hive-hydra
```

Then run it (from the repo root or worktree):

```sh
HIVE_HYDRA_BOT_NOKAMUTE_EASY_PASSWORD=nokamute-easy \
HIVE_HYDRA_BOT_NOKAMUTE_MEDIUM_PASSWORD=nokamute-medium \
HIVE_HYDRA_BOT_NOKAMUTE_HARD_PASSWORD=nokamute-hard \
  .cargo/target/release/hive-hydra \
  --config hive-hydra/hive-hydra.yaml
```

You should see all three bots authenticate successfully:

```
INFO  Authentication successful for bot: nokamute-easy
INFO  Authentication successful for bot: nokamute-medium
INFO  Authentication successful for bot: nokamute-hard
```

---

## 6. Test it

1. Open `http://localhost:3000` in a browser
2. Log in (or register a new account)
3. Click **Play Bot** and choose a difficulty
4. Play a move, then use the **Takeback** button
5. hive-hydra logs will show the bot auto-accepting or rejecting:
   - **Unrated games** (all Play Bot games): bot accepts, both your move and
     the bot's reply are undone so you can choose differently
   - **Rated games** (if you directly challenge a bot with rated=true): bot
     rejects

---

## How it all fits together

```
Browser  ←→  localhost:3000  (app container, cargo leptos watch)
                   ↕ SQL
             localhost:5433  (postgres container)
                   ↕ HTTP
           hive-hydra (native, this process)
                   ↕ stdio
           nokamute (spawned per move, native binary)
```

The **app** is volume-mounted from the hive repo directory, so editing source
files triggers a hot-reload inside the container. **hive-hydra** and
**nokamute** run natively on the host and connect to the app over HTTP.

---

## Stopping everything

```sh
# Stop the web app and postgres
docker compose down

# Kill hive-hydra (Ctrl-C in its terminal, or)
pkill hive-hydra
```
