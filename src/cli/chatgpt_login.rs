//! The ChatGPT sign-in commands of `mu auth`: login, logout, forget and test.

use anyhow::{Result, anyhow};
use mu_provider::auth::chatgpt::{self, PROVIDER_ID, USAGE_URL, oauth, signin};
use mu_provider::auth::store::Store;
use std::io::{IsTerminal, Write};
use tokio_util::sync::CancellationToken;

/// Signs in through the browser. On a terminal the pasted redirect URL is accepted too, for machines
/// the browser's callback cannot reach. The sign-in is saved only when it is the registered account.
pub(super) fn login(store: &Store, out: &mut impl Write) -> Result<i32> {
    let host = store.chatgpt_host_id()?;
    let endpoints = store.chatgpt_endpoints();
    let req = oauth::LoginRequest {
        endpoints: &endpoints,
        host_id: &host,
        known: store.chatgpt_known(),
        cancel: CancellationToken::new(),
    };
    let tty = std::io::stdin().is_terminal();
    // The sender lives until the sign-in ends: a closed channel would fail the wait for the browser.
    let (paste_tx, mut paste_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if tty {
        let tx = paste_tx.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lines().map_while(std::result::Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    let mut on_url = |url: &str| {
        eprintln!("Sign in with ChatGPT. A browser should open; if it does not, open this link:\n\n{url}\n");
        if tty {
            eprintln!("If the browser cannot reach this machine, paste the final redirect URL here and press Enter.");
        }
        let _ = chatgpt::browser::open_browser(url);
    };
    let mut notice = |text: &str| eprintln!("{text}");
    let rt = mu_base::runtime::current_thread_runtime()?;
    let account = match rt.block_on(oauth::login(req, &mut on_url, &mut notice, &mut paste_rx)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sign-in did not finish: {e}; nothing saved");
            return Ok(1);
        }
    };
    let saved = match signin::save(store, account) {
        Ok(s) => s,
        Err(m) => {
            eprintln!("{m}");
            return Ok(1);
        }
    };
    writeln!(out, "signed in as {}", saved.label)?;
    if !saved.plan_usage {
        eprintln!(
            "plan usage is not enabled for this sign-in, so {PROVIDER_ID}/* models cannot run yet. Run `mu auth login {PROVIDER_ID}` again and approve the plan permission."
        );
        return Ok(1);
    }
    writeln!(out, "{PROVIDER_ID}/* models now run on your ChatGPT plan.")?;
    if saved.welcome {
        writeln!(out, "You're using your ChatGPT plan. Manage usage: {USAGE_URL}")?;
    }
    Ok(0)
}

/// Drops the tokens, then revokes the refresh token at the issuer. `forget` drops the registration too.
pub(super) fn logout(store: &Store, out: &mut impl Write, forget: bool) -> Result<i32> {
    if forget {
        return forget_registration(store, out);
    }
    let Some(account) = signin::sign_out(store).map_err(|e| anyhow!("{e}"))? else {
        eprintln!("not signed in with ChatGPT");
        return Ok(1);
    };
    writeln!(
        out,
        "signed out of ChatGPT ({}); {PROVIDER_ID}/* models stop running until you sign in again",
        account.label()
    )?;
    if let Err(m) = mu_base::runtime::current_thread_runtime()?.block_on(signin::revoke(store, &account)) {
        eprintln!(
            "could not confirm that the sign-in was revoked ({m}). Disconnect mu under your ChatGPT settings, apps."
        );
    }
    Ok(0)
}

/// Drops the registration and its tokens, then revokes the refresh token at the issuer. The app stays
/// listed in ChatGPT settings, so that is said too.
pub(super) fn forget_registration(store: &Store, out: &mut impl Write) -> Result<i32> {
    let Some(account) = signin::forget(store).map_err(|e| anyhow!("{e}"))? else {
        eprintln!("no ChatGPT sign-in is saved");
        return Ok(1);
    };
    writeln!(
        out,
        "forgot the ChatGPT registration ({}); {PROVIDER_ID}/* models stop running until you sign in again",
        account.label()
    )?;
    writeln!(
        out,
        "The next `mu auth login {PROVIDER_ID}` registers a new client and may sign in as a different ChatGPT account."
    )?;
    writeln!(out, "The old app stays listed under your ChatGPT settings, apps, until you disconnect it there.")?;
    let revoked = if account.signed_in() {
        mu_base::runtime::current_thread_runtime()?.block_on(signin::revoke(store, &account))
    } else {
        Ok(())
    };
    if let Err(m) = revoked {
        eprintln!(
            "could not confirm that the sign-in was revoked ({m}). Disconnect mu under your ChatGPT settings, apps."
        );
    }
    Ok(0)
}

/// Asks the ChatGPT plan whether the sign-in works: the model list is fetched with it.
pub(super) fn chatgpt_test(store: &Store, out: &mut impl Write) -> Result<i32> {
    if !store.chatgpt_account().is_some_and(|a| a.signed_in()) {
        eprintln!("not signed in with ChatGPT; run `mu auth login {PROVIDER_ID}`");
        return Ok(1);
    }
    match mu_base::runtime::current_thread_runtime()?.block_on(chatgpt::store::check(store)) {
        Ok(models) => {
            writeln!(out, "{PROVIDER_ID}: the plan answers; {} models", models.len())?;
            Ok(0)
        }
        Err(m) => {
            eprintln!("{PROVIDER_ID}: {m}");
            Ok(1)
        }
    }
}
