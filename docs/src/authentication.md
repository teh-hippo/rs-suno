# Authentication

Suno has no public API and issues no API keys. `rs-suno` authenticates the same
way the Suno web app does: with your Clerk `__client` session cookie. You copy
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

The token lives in your browser once you are logged in to Suno. It belongs to
the **`auth.suno.com`** origin (not `suno.com`), and it is marked **HttpOnly**,
so no script or bookmarklet can read it. Your browser's developer tools show it
to you directly, on every platform. Pick your browser below and follow the
steps.

<div class="browser-picker" id="cookie-capture">
  <p class="browser-picker__label">Which browser are you using?</p>
  <div class="browser-picker__buttons">
    <button type="button" data-browser="chrome" aria-selected="true">Chrome</button>
    <button type="button" data-browser="edge" aria-selected="false">Microsoft Edge</button>
    <button type="button" data-browser="firefox" aria-selected="false">Firefox</button>
    <button type="button" data-browser="safari" aria-selected="false">Safari</button>
  </div>

  <div class="browser-steps" data-browser="chrome">
    <ol>
      <li>Sign in at <a href="https://suno.com">suno.com</a>.</li>
      <li>Open DevTools: press <kbd>F12</kbd> (or <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>I</kbd>; <kbd>Cmd</kbd>+<kbd>Option</kbd>+<kbd>I</kbd> on a Mac).</li>
      <li>Open <strong>Application &rsaquo; Storage &rsaquo; Cookies</strong> and select <code>https://auth.suno.com</code>.</li>
      <li>Type <code>__client</code> in the <strong>Filter</strong> box and click the <code>__client</code> row.</li>
      <li>Copy its <strong>Value</strong> (right-click the value, or select it and copy).</li>
    </ol>
  </div>

  <div class="browser-steps" data-browser="edge">
    <ol>
      <li>Sign in at <a href="https://suno.com">suno.com</a>.</li>
      <li>Open DevTools: press <kbd>F12</kbd> (or <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>I</kbd>).</li>
      <li>Open <strong>Application &rsaquo; Cookies</strong> and select <code>https://auth.suno.com</code>.</li>
      <li>Type <code>__client</code> in the <strong>Filter</strong> box and click the <code>__client</code> row.</li>
      <li>Copy its <strong>Value</strong>.</li>
    </ol>
  </div>

  <div class="browser-steps" data-browser="firefox">
    <ol>
      <li>Sign in at <a href="https://suno.com">suno.com</a>.</li>
      <li>Open DevTools: press <kbd>F12</kbd>.</li>
      <li>Open the <strong>Storage</strong> tab &rsaquo; <strong>Cookies</strong> and select <code>https://auth.suno.com</code>.</li>
      <li>Find the <code>__client</code> row in the list.</li>
      <li>Double-click its <strong>Value</strong>, then copy it.</li>
    </ol>
  </div>

  <div class="browser-steps" data-browser="safari">
    <ol>
      <li>Enable the tools once: <strong>Safari &rsaquo; Settings &rsaquo; Advanced</strong>, then tick <strong>Show features for web developers</strong>.</li>
      <li>Sign in at <a href="https://suno.com">suno.com</a>.</li>
      <li>Open <strong>Develop &rsaquo; Show Web Inspector</strong> (<kbd>Cmd</kbd>+<kbd>Option</kbd>+<kbd>I</kbd>).</li>
      <li>Open the <strong>Storage</strong> tab &rsaquo; <strong>Cookies</strong> and select the <code>auth.suno.com</code> domain.</li>
      <li>Find <code>__client</code> and copy its <strong>Value</strong>.</li>
    </ol>
  </div>
</div>

The `__client` row shows a tick in the **HttpOnly** column. That is expected:
the developer tools can display it even though page scripts cannot read it.

<details>
<summary>Cannot find the <code>auth.suno.com</code> entry? Use the Network tab (works in every browser)</summary>

1. Open DevTools and switch to the **Network** tab.
2. Reload the page, or open your Library, so requests appear.
3. Type `auth.suno.com` (or `client`) in the filter box.
4. Click the request to `auth.suno.com/v1/client`.
5. Under **Headers &rsaquo; Request Headers**, find the **Cookie** header and
   copy the whole value.

`rs-suno` keeps only `__client` from a full `Cookie:` header, so pasting the
entire header is fine.

</details>

On a phone, the developer tools are hidden, so route the browser through a
capturing proxy (for example [Stream](https://apps.apple.com/app/id1312141691)
on iOS), sign in, and copy the `Cookie` header sent to `auth.suno.com`.

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
   command, trims stdout, and uses that as the token. This is the recommended
   way to keep the token in a secret store (see below).
4. The `token` field in your [config file](configuration.md), which is the usual
   fallback place for it.

The interactive setup writes it to the config for you:

```bash
suno config init
```

See [Configuration](configuration.md) for the file format and for running
multiple accounts.

## Store the token securely

The token grants full access to your library, so prefer keeping it out of plain
text. Point `token_command` at your secret store and `rs-suno` reads the token
on each run, so nothing sensitive sits on disk.

**Linux (GNOME or KDE keyring, via libsecret).** Store it once, then read it on
demand:

```bash
secret-tool store --label="Suno __client" service suno account me
```

```toml
[accounts.me]
token_command = "secret-tool lookup service suno account me"
```

**macOS (Keychain).** Add the item once (you are prompted for the value), then
read it back:

```bash
security add-generic-password -a me -s suno-client -w
```

```toml
[accounts.me]
token_command = "security find-generic-password -a me -s suno-client -w"
```

**Windows and cross-platform managers.** Bitwarden, 1Password, and `pass` all
work as a `token_command`:

```toml
[accounts.me]
token_command = "bws secret get <secret-id>"          # Bitwarden Secrets Manager
# token_command = "op read op://Private/Suno/client"  # 1Password CLI
# token_command = "pass show suno/client"             # pass
```

If you must store the token in the config file instead, restrict its
permissions (for example `chmod 600`), and note that `suno config show` already
prints `[redacted]` in place of every token.

## Check and refresh a token

Confirm a stored token still works by re-minting its JWT:

```bash
suno auth refresh <account>
```

Or run the fuller diagnostics command:

```bash
suno doctor --account <account>
```

On success it prints the account and its display name. If the account label is
omitted, it uses your single configured account, or `--all` to check every one.
`doctor` also reports token-expiry state and the remaining credits balance.

Running `suno auth` with no subcommand opens this guide in your browser.

When a token stops working (you logged out, or Suno rotated the session), give
the account a fresh `__client` token. There is no update command, so edit the
account's `token` in your config file directly (run `suno version` to print the
resolved config path) and set it to the new `__client=<your-token>` value. If
the account uses `token_command` instead, a rotated secret is picked up on the
next run, because the command runs every time.

`suno config add-account <label>` is for adding a new account and refuses a
label that already exists, so it is not the way to update an existing token.

## Why not a bookmarklet, userscript, or extension?

Short answer: the browser blocks it. The `__client` cookie is marked
**HttpOnly**, so no bookmarklet and no Greasemonkey or Tampermonkey userscript
can read it through `document.cookie`. It is also scoped to the `auth.suno.com`
origin, not the `suno.com` page you browse, so a script running on the app could
not see it even if it were readable.

A browser extension *can* read HttpOnly cookies, but only per engine: Chrome,
Firefox, and Safari would each need a separately built and maintained add-on,
which is not worth it when the developer tools already expose the value on every
platform. That is why the steps above use them.

(The short-lived `__session` JWT *is* readable by scripts, but it expires within
minutes and cannot be refreshed without `__client`, so it is no use as a stored
credential.)

## Keeping the token safe

`rs-suno` never prints your token or a minted JWT:

- `suno config show` redacts every token, printing `[redacted]`.
- The `--token` value is never printed in `--help` output, logs, or errors.
- The `__client` cookie is only ever sent to Clerk; the Suno API only ever
  receives the short-lived JWT.

If you use `token_command`, remember that `rs-suno` executes a user-configured
shell command and trusts its stdout as the credential. Keep that command under
your control, avoid echoing secrets to stderr, and treat the command itself as
sensitive configuration.

Never commit a token to source control or paste it into logs or issues.
