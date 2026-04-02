# ~/.bitrouter

This is the BitRouter home directory. It stores configuration, credentials,
and runtime state for the `bitrouter` CLI.

## Directory Structure

| Path             | Description                                             |
| ---------------- | ------------------------------------------------------- |
| `bitrouter.yaml` | Main configuration file (providers, models, server)     |
| `.env`           | API keys and secrets (loaded automatically at startup)  |
| `.keys/`         | MasterKeypair files for on-chain identity and payments  |
| `bitrouter.db`   | Local SQLite database (accounts, sessions, messages)    |
| `logs/`          | Server log files                                        |
| `run/`           | PID files and Unix sockets for daemon management        |
| `.gitignore`     | Prevents secrets and runtime files from being committed |

## Security

The `.env` and `.keys/` directories contain sensitive credentials.
They are excluded from version control by `.gitignore`. **Do not share
or commit these files.**

## More Information

- Documentation: <https://github.com/bitrouter/bitrouter>
- Configuration reference: <https://github.com/bitrouter/bitrouter#configuration>
