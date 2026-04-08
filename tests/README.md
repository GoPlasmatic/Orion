# Orion Test Suite

## Unit and Integration Tests

Run all standard tests (no external services required):

```bash
cargo test
```

These tests use an in-memory SQLite database and require no additional setup.

## External Service Tests

Some tests exercise real database and cache connectors (PostgreSQL, MySQL, MongoDB, Redis). These are marked `#[ignore]` so they are skipped during normal `cargo test` runs.

### Start External Services

```bash
docker compose -f docker-compose.test.yml up -d
```

Wait for all services to become healthy:

```bash
docker compose -f docker-compose.test.yml ps
```

### Run All External Tests

```bash
cargo test -- --ignored
```

### Run a Specific External Test

```bash
cargo test test_postgres_db_write_and_read -- --ignored
cargo test test_mysql -- --ignored
cargo test test_redis -- --ignored
cargo test test_mongo -- --ignored
```

### Run a Specific Test File

```bash
cargo test --test postgres_test -- --ignored
cargo test --test mysql_test -- --ignored
cargo test --test connector_redis_test -- --ignored
cargo test --test mongodb_test -- --ignored
```

### Stop External Services

```bash
docker compose -f docker-compose.test.yml down
```

## Service Ports

| Service    | Port  | Credentials                  |
|------------|-------|------------------------------|
| PostgreSQL | 5432  | postgres / test              |
| MySQL      | 3306  | root / test                  |
| Redis      | 6379  | (no auth)                    |
| MongoDB    | 27017 | (no auth)                    |
