# Authentication

Suno has no public API and issues no API keys. `rs-suno` authenticates the same
way the Suno web app does: with your Clerk `__client` session cookie. You paste
that cookie into `rs-suno` once, and it mints the short-lived tokens it needs
from there.

## How it works

- You supply your `__client` session token (a long string).
- On each run, `rs-suno` sends that token to Clerk (`auth.suno.com`) and mints a
  short-lived JSON Web Token (JWT).
- It refreshes the JWT automatically, shortly before it expires, so long runs do
  not stall.
- Only the minted JWT is sent to the Suno API. Your `__client` cookie is sent
  only to Clerk, never to Suno's API host.

If authentication fails partway through a run, `rs-suno` stops that account
cleanly rather than hammering the server, and re-authenticates on the next run.

## Get your `__client` token

The token lives in your browser once you are logged in to Suno:

1. Log in at [suno.com](https://suno.com) in your browser.
2. Open the browser developer tools (F12 on most browsers).
3. Go to the storage or application panel and find **Cookies**.
4. Select the Suno/Clerk origin and copy the value of the cookie named
   `__client`.

`rs-suno` accepts the token in whichever form is convenient: the raw value, a
`__client=<value>` assignment, or the full `Cookie:` header string. Treat this
value like a password. Anyone with it can access your library.

## Provide the token

You can supply the token four ways, in order of precedence:

1. The `--token <TOKEN>` flag.
2. The `SUNO_TOKEN` environment variable (or the per-account
   `SUNO_<LABEL>_TOKEN`).
3. A `token_command`, from `SUNO_TOKEN_COMMAND`, `SUNO_<LABEL>_TOKEN_COMMAND`,
   or your [config file](configuration.md). `rs-suno` runs the configured shell
   command, trims stdout, and uses that as the token.
4. The `token` field in your [config file](configuration.md), which is the usual
   fallback place for it.

For example, Bitwarden Secrets Manager works natively with:

```toml
[accounts.me]
token_command = "bws secret list -o json | jq -r '[.[]|select(.key==\"SUNO_TOKEN\")][0].value'"
```

or just for one run:

```bash
SUNO_TOKEN_COMMAND="bws secret get <secret-id>" suno sync
```

The interactive setup writes it to the config for you:

```bash
suno config init
```

See [Configuration](configuration.md) for the file format and for running
multiple accounts.

## Check and refresh a token

Confirm a stored token still works by re-minting its JWT:

```bash
suno auth refresh <account>
```

On success it prints the account and its display name. If the account label is
omitted, it uses your single configured account, or `--all` to check every one.

When a token stops working (you logged out, or Suno rotated the session), update
it:

```bash
suno config add-account <account> --token <new-token>
```

## Keeping the token safe

`rs-suno` never prints your token or a minted JWT:

- `suno config show` redacts every token, printing `[redacted]`.
- The `--token` flag hides its environment value in help output.
- The `__client` cookie is only ever sent to Clerk; the Suno API only ever
  receives the short-lived JWT.

If you use `token_command`, remember that `rs-suno` executes a user-configured
shell command and trusts its stdout as the credential. Keep that command under
your control, avoid echoing secrets to stderr, and treat the command itself as
sensitive configuration.

Never commit a token to source control or paste it into logs or issues.
