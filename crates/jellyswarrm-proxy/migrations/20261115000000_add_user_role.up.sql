-- The management UI's administrator is now a real user. The role decides what the
-- UI offers; it does not make the user a backend administrator.
ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'user' CHECK (role IN ('user', 'admin'));
