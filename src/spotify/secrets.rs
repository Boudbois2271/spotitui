//! The refresh token in the system keyring (Secret Service), read and written
//! asynchronously over the session bus.
//!
//! The item is the one the `keyring` crate used to write, so a login stored by
//! an earlier spotitui keeps working: same attributes, same label.

use super::Error;
use secret_service::{Collection, EncryptionType, Item, SecretService};
use std::collections::HashMap;

const SERVICE: &str = "spotitui";
const USERNAME: &str = "refresh-token";
const TARGET: &str = "default";
const LABEL: &str = "refresh-token@spotitui:default (keyring v3.6.3)";
const CONTENT_TYPE: &str = "text/plain";

fn search_attributes() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("service", SERVICE),
        ("username", USERNAME),
        ("target", TARGET),
    ])
}

async fn connect() -> Result<SecretService<'static>, Error> {
    Ok(SecretService::connect(EncryptionType::Dh).await?)
}

/// Every stored item for the login, unlocked or not.
async fn items<'a>(service: &'a SecretService<'a>) -> Result<Vec<Item<'a>>, Error> {
    let found = service.search_items(search_attributes()).await?;
    Ok(found.unlocked.into_iter().chain(found.locked).collect())
}

/// Whether a locked keyring may show its password prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unlock {
    /// Show the prompt (gnome-keyring's "unlock your keyring" dialog).
    Ask,
    /// Fail with [`Error::KeyringLocked`] instead, for background work.
    Never,
}

/// Unlocks an item, asking for the keyring's password when allowed. A
/// dismissed prompt leaves it locked.
async fn unlock_item(item: &Item<'_>, unlock: Unlock) -> Result<(), Error> {
    if !item.is_locked().await? {
        return Ok(());
    }
    if unlock == Unlock::Never {
        return Err(Error::KeyringLocked);
    }
    item.unlock().await?;
    if item.is_locked().await? {
        return Err(Error::KeyringLocked);
    }
    Ok(())
}

async fn unlock_collection(collection: &Collection<'_>, unlock: Unlock) -> Result<(), Error> {
    if !collection.is_locked().await? {
        return Ok(());
    }
    if unlock == Unlock::Never {
        return Err(Error::KeyringLocked);
    }
    collection.unlock().await?;
    if collection.is_locked().await? {
        return Err(Error::KeyringLocked);
    }
    Ok(())
}

/// The stored refresh token; `None` when not logged in.
pub async fn load(unlock: Unlock) -> Result<Option<String>, Error> {
    let service = connect().await?;
    let Some(item) = items(&service).await?.into_iter().next() else {
        return Ok(None);
    };
    unlock_item(&item, unlock).await?;
    let secret = item.get_secret().await?;
    String::from_utf8(secret)
        .map(Some)
        .map_err(|_| Error::Response("the stored login isn't text".into()))
}

pub async fn store(token: &str, unlock: Unlock) -> Result<(), Error> {
    let service = connect().await?;
    if let Some(item) = items(&service).await?.into_iter().next() {
        unlock_item(&item, unlock).await?;
        item.set_secret(token.as_bytes(), CONTENT_TYPE).await?;
        return Ok(());
    }
    let collection = service.get_default_collection().await?;
    unlock_collection(&collection, unlock).await?;
    let mut attributes = search_attributes();
    attributes.insert("application", "rust-keyring");
    collection
        .create_item(LABEL, attributes, token.as_bytes(), true, CONTENT_TYPE)
        .await?;
    Ok(())
}

/// Removes the login; doing it twice is fine.
pub async fn forget() -> Result<(), Error> {
    let service = connect().await?;
    for item in items(&service).await? {
        unlock_item(&item, Unlock::Ask).await?;
        item.delete().await?;
    }
    Ok(())
}
