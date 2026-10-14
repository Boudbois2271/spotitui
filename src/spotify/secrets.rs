//! The refresh token in the system keyring (Secret Service), read and written
//! asynchronously over the session bus.
//!
//! The item is the one the `keyring` crate used to write, so a login stored by
//! an earlier spotitui keeps working: same attributes, same label.

use super::Error;
use secret_service::{EncryptionType, Item, SecretService};
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

/// The stored refresh token; `None` when not logged in.
pub async fn load() -> Result<Option<String>, Error> {
    let service = connect().await?;
    let Some(item) = items(&service).await?.into_iter().next() else {
        return Ok(None);
    };
    item.ensure_unlocked().await?;
    let secret = item.get_secret().await?;
    String::from_utf8(secret)
        .map(Some)
        .map_err(|_| Error::Response("the stored login isn't text".into()))
}

pub async fn store(token: &str) -> Result<(), Error> {
    let service = connect().await?;
    if let Some(item) = items(&service).await?.into_iter().next() {
        item.ensure_unlocked().await?;
        item.set_secret(token.as_bytes(), CONTENT_TYPE).await?;
        return Ok(());
    }
    let collection = service.get_default_collection().await?;
    collection.ensure_unlocked().await?;
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
        item.delete().await?;
    }
    Ok(())
}
