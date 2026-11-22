//! Binds for the wizards (ChatGPT login, custom providers, MCP add and login, API-key entry).

binds! {
    /// Give up on the running wizard or sign-in.
    WIZARD_CANCEL = "wizard", "cancel", ["esc"];
}
