-- MatrixMedia mm-postgres init -- rendered by deploy/lib/render.sh
-- Creates mm_app + mm_admin roles and matrixmedia DB.
-- IMPORTANT: this file contains rendered passwords; store at /opt/mm/config/
-- with mode 600 and ensure the directory is not world-readable.

-- Application role: minimum permissions for runtime
CREATE ROLE mm_app LOGIN PASSWORD '${POSTGRES_APP_PASS}' NOSUPERUSER NOCREATEDB NOCREATEROLE;
-- Admin role: used only for migrations
CREATE ROLE mm_admin LOGIN PASSWORD '${POSTGRES_APP_ADMIN_PASS}' NOSUPERUSER CREATEDB NOCREATEROLE;
-- Create the database owned by admin
CREATE DATABASE matrixmedia OWNER mm_admin ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0;

\connect matrixmedia

-- Revoke default permissive public schema access
REVOKE ALL ON SCHEMA public FROM PUBLIC;
GRANT ALL ON SCHEMA public TO mm_admin;
GRANT USAGE ON SCHEMA public TO mm_app;

-- Default privileges: objects mm_admin creates become RW for mm_app
ALTER DEFAULT PRIVILEGES FOR ROLE mm_admin IN SCHEMA public
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO mm_app;
ALTER DEFAULT PRIVILEGES FOR ROLE mm_admin IN SCHEMA public
  GRANT USAGE, SELECT ON SEQUENCES TO mm_app;

-- mm_app CANNOT create new objects
REVOKE CREATE ON SCHEMA public FROM mm_app;
