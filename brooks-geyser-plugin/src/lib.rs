use {
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPlugin, ReplicaAccountInfoV3, ReplicaAccountInfoVersions, Result as PluginResult,
    },
    solana_pubkey::Pubkey,
    std::{
        fmt,
        sync::atomic::{AtomicU64, Ordering},
    },
};

/// brooks dummy docs
///
/// # Safety
///
/// :)
#[no_mangle]
#[allow(improper_ctypes_definitions)]
pub unsafe extern "C" fn _create_plugin() -> *mut dyn GeyserPlugin {
    let plugin = Plugin::default();
    let plugin: Box<dyn GeyserPlugin> = Box::new(plugin);
    Box::into_raw(plugin)
}

#[derive(Debug, Default)]
pub struct Plugin {
    inner: Option<PluginInner>,
}

impl GeyserPlugin for Plugin {
    fn name(&self) -> &'static str {
        "brooks-geyser-plugin"
    }

    fn on_load(&mut self, _config_file: &str, is_reload: bool) -> PluginResult<()> {
        agave_logger::setup_with_default("brooks_geyser_plugin=info");
        log::info!("brooks DEBUG: on_load() is reload: {is_reload}");
        self.inner = Some(PluginInner::default());
        Ok(())
    }

    fn update_account(
        &self,
        account: ReplicaAccountInfoVersions,
        slot: u64,
        is_startup: bool,
    ) -> PluginResult<()> {
        let plugin = self.inner.as_ref().unwrap();
        plugin.startup_num_accounts.fetch_add(1, Ordering::Relaxed);
        let account = match account {
            ReplicaAccountInfoVersions::V0_0_3(account) => account,
            _ => unimplemented!(),
        };
        log::trace!("brooks DEBUG: update_account() is startup: {is_startup}, slot: {slot}, account: {account:?}");
        if !is_startup && account.lamports == 0 {
            log::debug!(
                "brooks DEBUG: account was closed in slot: {slot}, {}",
                AccountLogger(account),
            );
        }
        Ok(())
    }

    fn notify_end_of_startup(&self) -> PluginResult<()> {
        let plugin = self.inner.as_ref().unwrap();
        log::info!(
            "brooks DEBUG: notify_end_of_startup() num accounts: {}",
            plugin.startup_num_accounts.load(Ordering::Relaxed)
        );
        Ok(())
    }

    fn account_data_notifications_enabled(&self) -> bool {
        true // must be true to get account snapshot notifications
    }

    fn account_data_snapshot_notifications_enabled(&self) -> bool {
        true
    }

    fn transaction_notifications_enabled(&self) -> bool {
        false
    }

    fn entry_notifications_enabled(&self) -> bool {
        false
    }
}

#[derive(Debug, Default)]
struct PluginInner {
    startup_num_accounts: AtomicU64,
}

struct AccountLogger<'a>(&'a ReplicaAccountInfoV3<'a>);

impl fmt::Display for AccountLogger<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let pubkey = Pubkey::try_from(self.0.pubkey).unwrap();
        let owner = Pubkey::try_from(self.0.owner).unwrap();
        f.debug_struct("Account")
            .field("pubkey", &pubkey)
            .field("lamports", &self.0.lamports)
            .field("data_len", &self.0.data.len())
            .field("owner", &owner)
            .finish_non_exhaustive()
    }
}
