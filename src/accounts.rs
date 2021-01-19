use std::collections::BTreeMap;

use async_std::fs;
use async_std::path::PathBuf;
use async_std::prelude::*;
use async_std::sync::{Arc, RwLock};
use uuid::Uuid;

use anyhow::{ensure, Context as _};
use serde::{Deserialize, Serialize};

use crate::context::Context;
use crate::error::Result;
use crate::events::Event;

/// Account manager, that can handle multiple accounts in a single place.
#[derive(Debug, Clone)]
pub struct Accounts {
    dir: PathBuf,
    config: Config,
    accounts: Arc<RwLock<BTreeMap<u32, Context>>>,
}

impl Accounts {
    /// Loads or creates an accounts folder at the given `dir`.
    pub async fn new(os_name: String, dir: PathBuf) -> Result<Self> {
        if !dir.exists().await {
            Accounts::create(os_name, &dir).await?;
        }

        Accounts::open(dir).await
    }

    /// Creates a new default structure, including a default account.
    pub async fn create(os_name: String, dir: &PathBuf) -> Result<()> {
        fs::create_dir_all(dir)
            .await
            .context("failed to create folder")?;

        // create default account
        let config = Config::new(os_name.clone(), dir).await?;
        let account_config = config.new_account(dir).await?;

        Context::new(os_name, account_config.dbfile().into(), account_config.id)
            .await
            .context("failed to create default account")?;

        Ok(())
    }

    /// Opens an existing accounts structure. Will error if the folder doesn't exist,
    /// no account exists and no config exists.
    pub async fn open(dir: PathBuf) -> Result<Self> {
        ensure!(dir.exists().await, "directory does not exist");

        let config_file = dir.join(CONFIG_NAME);
        ensure!(config_file.exists().await, "accounts.toml does not exist");

        let config = Config::from_file(config_file).await?;
        let accounts = config.load_accounts().await?;

        Ok(Self {
            dir,
            config,
            accounts: Arc::new(RwLock::new(accounts)),
        })
    }

    /// Get an account by its `id`:
    pub async fn get_account(&self, id: u32) -> Option<Context> {
        self.accounts.read().await.get(&id).cloned()
    }

    /// Get the currently selected account.
    pub async fn get_selected_account(&self) -> Context {
        let id = self.config.get_selected_account().await;
        self.accounts
            .read()
            .await
            .get(&id)
            .cloned()
            .expect("inconsistent state")
    }

    /// Select the given account.
    pub async fn select_account(&self, id: u32) -> Result<()> {
        self.config.select_account(id).await?;

        Ok(())
    }

    /// Add a new account.
    pub async fn add_account(&self) -> Result<u32> {
        let os_name = self.config.os_name().await;
        let account_config = self.config.new_account(&self.dir).await?;

        let ctx = Context::new(os_name, account_config.dbfile().into(), account_config.id).await?;
        self.accounts.write().await.insert(account_config.id, ctx);

        Ok(account_config.id)
    }

    /// Remove an account.
    pub async fn remove_account(&self, id: u32) -> Result<()> {
        let ctx = self.accounts.write().await.remove(&id);
        ensure!(ctx.is_some(), "no account with this id: {}", id);
        let ctx = ctx.unwrap();
        ctx.stop_io().await;
        drop(ctx);

        if let Some(cfg) = self.config.get_account(id).await {
            fs::remove_dir_all(async_std::path::PathBuf::from(&cfg.dir))
                .await
                .context("failed to remove account data")?;
        }
        self.config.remove_account(id).await?;

        Ok(())
    }

    /// Migrate an existing account into this structure.
    pub async fn migrate_account(&self, dbfile: PathBuf) -> Result<u32> {
        let blobdir = Context::derive_blobdir(&dbfile);

        ensure!(
            dbfile.exists().await,
            "no database found: {}",
            dbfile.display()
        );
        ensure!(
            blobdir.exists().await,
            "no blobdir found: {}",
            blobdir.display()
        );

        let old_id = self.config.get_selected_account().await;

        // create new account
        let account_config = self.config.new_account(&self.dir).await?;

        let new_dbfile = account_config.dbfile().into();
        let new_blobdir = Context::derive_blobdir(&new_dbfile);

        let res = {
            fs::create_dir_all(&account_config.dir).await?;
            fs::rename(&dbfile, &new_dbfile).await?;
            fs::rename(&blobdir, &new_blobdir).await?;
            Ok(())
        };

        match res {
            Ok(_) => {
                let ctx = Context::with_blobdir(
                    self.config.os_name().await,
                    new_dbfile,
                    new_blobdir,
                    account_config.id,
                )
                .await?;
                self.accounts.write().await.insert(account_config.id, ctx);
                Ok(account_config.id)
            }
            Err(err) => {
                // remove temp account
                fs::remove_dir_all(async_std::path::PathBuf::from(&account_config.dir))
                    .await
                    .context("failed to remove account data")?;

                self.config.remove_account(account_config.id).await?;

                // set selection back
                self.select_account(old_id).await?;

                Err(err)
            }
        }
    }

    /// Get a list of all account ids.
    pub async fn get_all(&self) -> Vec<u32> {
        self.accounts.read().await.keys().copied().collect()
    }

    /// Import a backup using a new account and selects it.
    pub async fn import_account(&self, file: PathBuf) -> Result<u32> {
        let old_id = self.config.get_selected_account().await;

        let id = self.add_account().await?;
        let ctx = self.get_account(id).await.expect("just added");

        match crate::imex::imex(&ctx, crate::imex::ImexMode::ImportBackup, &file).await {
            Ok(_) => Ok(id),
            Err(err) => {
                // remove temp account
                self.remove_account(id).await?;
                // set selection back
                self.select_account(old_id).await?;
                Err(err)
            }
        }
    }

    pub async fn start_io(&self) {
        let accounts = &*self.accounts.read().await;
        for account in accounts.values() {
            account.start_io().await;
        }
    }

    pub async fn stop_io(&self) {
        let accounts = &*self.accounts.read().await;
        for account in accounts.values() {
            account.stop_io().await;
        }
    }

    pub async fn maybe_network(&self) {
        let accounts = &*self.accounts.read().await;
        for account in accounts.values() {
            account.maybe_network().await;
        }
    }

    /// Unified event emitter.
    pub async fn get_event_emitter(&self) -> EventEmitter {
        let emitters: Vec<_> = self
            .accounts
            .read()
            .await
            .iter()
            .map(|(_id, a)| a.get_event_emitter())
            .collect();

        EventEmitter(futures::stream::select_all(emitters))
    }
}

#[derive(Debug)]
pub struct EventEmitter(futures::stream::SelectAll<crate::events::EventEmitter>);

impl EventEmitter {
    /// Blocking recv of an event. Return `None` if all `Sender`s have been droped.
    pub fn recv_sync(&mut self) -> Option<Event> {
        async_std::task::block_on(self.recv())
    }

    /// Async recv of an event. Return `None` if all `Sender`s have been droped.
    pub async fn recv(&mut self) -> Option<Event> {
        self.0.next().await
    }
}

impl async_std::stream::Stream for EventEmitter {
    type Item = Event;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.0).poll_next(cx)
    }
}

pub const CONFIG_NAME: &str = "accounts.toml";
pub const DB_NAME: &str = "dc.db";

#[derive(Debug, Clone)]
pub struct Config {
    file: PathBuf,
    inner: Arc<RwLock<InnerConfig>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct InnerConfig {
    pub os_name: String,
    /// The currently selected account.
    pub selected_account: u32,
    pub next_id: u32,
    pub accounts: Vec<AccountConfig>,
}

impl Config {
    pub async fn new(os_name: String, dir: &PathBuf) -> Result<Self> {
        let cfg = Config {
            file: dir.join(CONFIG_NAME),
            inner: Arc::new(RwLock::new(InnerConfig {
                os_name,
                accounts: Vec::new(),
                selected_account: 0,
                next_id: 1,
            })),
        };

        cfg.sync().await?;

        Ok(cfg)
    }

    pub async fn os_name(&self) -> String {
        self.inner.read().await.os_name.clone()
    }

    /// Sync the inmemory representation to disk.
    async fn sync(&self) -> Result<()> {
        fs::write(
            &self.file,
            toml::to_string_pretty(&*self.inner.read().await)?,
        )
        .await
        .context("failed to write config")
    }

    /// Read a configuration from the given file into memory.
    pub async fn from_file(file: PathBuf) -> Result<Self> {
        let bytes = fs::read(&file).await.context("failed to read file")?;
        let inner: InnerConfig = toml::from_slice(&bytes).context("failed to parse config")?;

        Ok(Config {
            file,
            inner: Arc::new(RwLock::new(inner)),
        })
    }

    pub async fn load_accounts(&self) -> Result<BTreeMap<u32, Context>> {
        let cfg = &*self.inner.read().await;
        let mut accounts = BTreeMap::new();
        for account_config in &cfg.accounts {
            let ctx = Context::new(
                cfg.os_name.clone(),
                account_config.dbfile().into(),
                account_config.id,
            )
            .await?;
            accounts.insert(account_config.id, ctx);
        }

        Ok(accounts)
    }

    /// Create a new account in the given root directory.
    pub async fn new_account(&self, dir: &PathBuf) -> Result<AccountConfig> {
        let id = {
            let inner = &mut self.inner.write().await;
            let id = inner.next_id;
            let uuid = Uuid::new_v4();
            let target_dir = dir.join(uuid.to_simple_ref().to_string());

            inner.accounts.push(AccountConfig {
                id,
                dir: target_dir.into(),
                uuid,
            });
            inner.next_id += 1;
            id
        };

        self.sync().await?;

        self.select_account(id).await.expect("just added");
        let cfg = self.get_account(id).await.expect("just added");
        Ok(cfg)
    }

    /// Removes an existing acccount entirely.
    pub async fn remove_account(&self, id: u32) -> Result<()> {
        {
            let inner = &mut *self.inner.write().await;
            if let Some(idx) = inner.accounts.iter().position(|e| e.id == id) {
                // remove account from the configs
                inner.accounts.remove(idx);
            }
            if inner.selected_account == id {
                // reset selected account
                inner.selected_account = inner.accounts.get(0).map(|e| e.id).unwrap_or_default();
            }
        }

        self.sync().await
    }

    pub async fn get_account(&self, id: u32) -> Option<AccountConfig> {
        self.inner
            .read()
            .await
            .accounts
            .iter()
            .find(|e| e.id == id)
            .cloned()
    }

    pub async fn get_selected_account(&self) -> u32 {
        self.inner.read().await.selected_account
    }

    pub async fn select_account(&self, id: u32) -> Result<()> {
        {
            let inner = &mut *self.inner.write().await;
            ensure!(
                inner.accounts.iter().any(|e| e.id == id),
                "invalid account id: {}",
                id
            );

            inner.selected_account = id;
        }

        self.sync().await?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AccountConfig {
    /// Unique id.
    pub id: u32,
    /// Root directory for all data for this account.
    pub dir: std::path::PathBuf,
    pub uuid: Uuid,
}

impl AccountConfig {
    /// Get the canoncial dbfile name for this configuration.
    pub fn dbfile(&self) -> std::path::PathBuf {
        self.dir.join(DB_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[async_std::test]
    async fn test_account_new_open() {
        let dir = tempfile::tempdir().unwrap();
        let p: PathBuf = dir.path().join("accounts1").into();

        let accounts1 = Accounts::new("my_os".into(), p.clone()).await.unwrap();
        let accounts2 = Accounts::open(p).await.unwrap();

        assert_eq!(accounts1.accounts.read().await.len(), 1);
        assert_eq!(accounts1.config.get_selected_account().await, 1);

        assert_eq!(accounts1.dir, accounts2.dir);
        assert_eq!(
            &*accounts1.config.inner.read().await,
            &*accounts2.config.inner.read().await,
        );
        assert_eq!(
            accounts1.accounts.read().await.len(),
            accounts2.accounts.read().await.len()
        );
    }

    #[async_std::test]
    async fn test_account_new_add_remove() {
        let dir = tempfile::tempdir().unwrap();
        let p: PathBuf = dir.path().join("accounts").into();

        let accounts = Accounts::new("my_os".into(), p.clone()).await.unwrap();

        assert_eq!(accounts.accounts.read().await.len(), 1);
        assert_eq!(accounts.config.get_selected_account().await, 1);

        let id = accounts.add_account().await.unwrap();
        assert_eq!(id, 2);
        assert_eq!(accounts.config.get_selected_account().await, id);
        assert_eq!(accounts.accounts.read().await.len(), 2);

        accounts.select_account(1).await.unwrap();
        assert_eq!(accounts.config.get_selected_account().await, 1);

        accounts.remove_account(1).await.unwrap();
        assert_eq!(accounts.config.get_selected_account().await, 2);
        assert_eq!(accounts.accounts.read().await.len(), 1);
    }

    #[async_std::test]
    async fn test_migrate_account() {
        let dir = tempfile::tempdir().unwrap();
        let p: PathBuf = dir.path().join("accounts").into();

        let accounts = Accounts::new("my_os".into(), p.clone()).await.unwrap();
        assert_eq!(accounts.accounts.read().await.len(), 1);
        assert_eq!(accounts.config.get_selected_account().await, 1);

        let extern_dbfile: PathBuf = dir.path().join("other").into();
        let ctx = Context::new("my_os".into(), extern_dbfile.clone(), 0)
            .await
            .unwrap();
        ctx.set_config(crate::config::Config::Addr, Some("me@mail.com"))
            .await
            .unwrap();

        drop(ctx);

        accounts
            .migrate_account(extern_dbfile.clone())
            .await
            .unwrap();
        assert_eq!(accounts.accounts.read().await.len(), 2);
        assert_eq!(accounts.config.get_selected_account().await, 2);

        let ctx = accounts.get_selected_account().await;
        assert_eq!(
            "me@mail.com",
            ctx.get_config(crate::config::Config::Addr).await.unwrap()
        );
    }

    /// Tests that accounts are sorted by ID.
    #[async_std::test]
    async fn test_accounts_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let p: PathBuf = dir.path().join("accounts").into();

        let accounts = Accounts::new("my_os".into(), p.clone()).await.unwrap();

        for expected_id in 2..10 {
            let id = accounts.add_account().await.unwrap();
            assert_eq!(id, expected_id);
        }

        let ids = accounts.get_all().await;
        for (i, expected_id) in (1..10).enumerate() {
            assert_eq!(ids.get(i), Some(&expected_id));
        }
    }
}
