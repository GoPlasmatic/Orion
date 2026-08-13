Create a database backup of the Orion server. This triggers a SQLite VACUUM INTO operation that creates a consistent snapshot of the database.

Returns the backup filename, file path, size in bytes, and creation timestamp. Backups are stored in the server's configured backup directory.

Note: This operation is only supported when the server uses SQLite as its database backend.
