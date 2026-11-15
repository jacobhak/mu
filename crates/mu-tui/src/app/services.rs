//! What `App` still holds of the provider registry: nothing it uses outside tests. Goes with the last
//! test that reads the registry directly.

use mu_provider::provider::client::Providers;
use std::rc::Rc;

pub(super) struct Services {
    pub(super) providers: Rc<Providers>,
}
