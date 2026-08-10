-- Seed schema + data for the postgres-orders example.
-- Loaded automatically by the postgres container on first start
-- (mounted into /docker-entrypoint-initdb.d/).

CREATE TABLE customers (
    id    SERIAL PRIMARY KEY,
    name  TEXT NOT NULL,
    email TEXT NOT NULL UNIQUE
);

CREATE TABLE orders (
    id          SERIAL PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers (id),
    item        TEXT NOT NULL,
    total       DOUBLE PRECISION NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO customers (name, email) VALUES
    ('Ada Lovelace', 'ada@example.com'),
    ('Grace Hopper', 'grace@example.com');

INSERT INTO orders (customer_id, item, total) VALUES
    (1, 'Analytical Engine Manual', 120.00),
    (1, 'Punch Card Set', 35.50),
    (2, 'COBOL Compiler License', 900.00);
