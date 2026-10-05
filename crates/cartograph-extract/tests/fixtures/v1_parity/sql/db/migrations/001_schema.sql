CREATE SCHEMA reporting;

CREATE TABLE users (
  id INT PRIMARY KEY,
  email VARCHAR(255) NOT NULL
);

CREATE TABLE orders (
  id INT PRIMARY KEY,
  user_id INT REFERENCES users(id),
  status VARCHAR(32)
);

CREATE TABLE order_items (
  id INT,
  order_id INT,
  sku VARCHAR(64),
  CONSTRAINT fk_order FOREIGN KEY (order_id) REFERENCES orders(id) ON DELETE CASCADE
);

CREATE TABLE reporting.events (
  id INT,
  user_id INT REFERENCES users(id)
);

CREATE TABLE audit_log (
  id INT,
  message TEXT
);

CREATE INDEX idx_users_email ON users(email);
