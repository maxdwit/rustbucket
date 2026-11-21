# Admin CLI

All commands run inside the container, e.g. `docker exec -i rustbucket rustbucket <command>` (`make shell` for a shell in the container).
Passwords are read from **stdin** (first line); they never appear in a process list.

| Command | What it does |
|---|---|
| `serve` | the HTTP server (default of the image) |
| `migrate` | apply pending database migrations |
| `healthcheck` | exit 0 if the server answers (used by the container healthcheck) |
| `user create <uid> [--display-name N] [--admin]` | new account; password from stdin |
| `user modify <uid> [--display-name N] [--email E] [--password-stdin] [--admin\|--no-admin] [--enable\|--disable]` | change an account. The last enabled administrator cannot be demoted or disabled. Disabling closes every way in (web sessions, app passwords, password) and keeps the files. |
| `user list` | accounts with email, role and state |
| `group create <gid>` / `group add-member <gid> <uid>` | groups (also manageable in the web UI under Users) |
| `scan [<uid>] [--dry-run] [--force]` | reconcile the index with the disk (like `occ files:scan`): imports files added by hand, drops index rows of vanished files. Refuses when it would remove most of the index (wrong mount); `--force` overrides. |
| `backup <file>` | consistent online snapshot of the database (`VACUUM INTO`) |
| `trash list <uid>` / `trash empty <uid> [--name N]` | inspect / empty a user's trash (optionally only entries with that original name) |
| `expire` | apply trash/version retention now (the server also does it hourly) |

Accounts can also be managed in the web UI (**Users**, administrators only). Account names: 1-64 characters of
letters, digits, space and `_ . @ -`, not starting with a dot (the name is the folder name on disk).
