CREATE VIEW active_users AS SELECT id FROM users;

CREATE VIEW user_orders AS
  SELECT u.id, COUNT(o.id) AS n
  FROM users u
  LEFT JOIN orders o ON o.user_id = u.id;

CREATE VIEW double_users AS
  SELECT * FROM users JOIN users u2 ON u2.id = users.id;

CREATE VIEW nested_orders AS
  SELECT * FROM (SELECT id FROM users) u JOIN orders o ON o.user_id = u.id;

CREATE VIEW reporting.daily_events AS
  SELECT * FROM reporting.events;
