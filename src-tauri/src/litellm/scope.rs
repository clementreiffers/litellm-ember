//! Portée des requêtes de consommation, partagée entre les appels du client.
use super::Client;
use futures_util::{
    future::{BoxFuture, Shared},
    FutureExt,
};
use sha2::{Digest, Sha256};
use std::time::Duration;

#[derive(Clone)]
pub(super) struct Scope {
    pub user_id: Option<String>,
    pub key_hash: Option<String>,
}

impl Scope {
    fn new(user_id: Option<String>, key: &str) -> Self {
        let key_hash = user_id
            .as_ref()
            .map(|_| format!("{:x}", Sha256::digest(key.as_bytes())));
        Self { user_id, key_hash }
    }

    pub fn append_key(&self, path: String) -> String {
        match &self.key_hash {
            Some(hash) => format!("{path}&api_key={hash}"),
            None => path,
        }
    }
}

type Lookup = Shared<BoxFuture<'static, Result<Scope, String>>>;
#[derive(Default)]
pub(super) enum State {
    #[default]
    Unknown,
    Loading(u64, Lookup),
    Ready(Scope),
}

#[derive(Default)]
pub(super) struct Cache {
    version: u64,
    seed_version: u64,
    state: State,
}

#[cfg(test)]
impl Cache {
    pub(super) fn test_user(key: &str) -> Self {
        Self {
            version: 1,
            seed_version: 1,
            state: State::Ready(Scope::new(Some("test-user".into()), key)),
        }
    }
}

impl Client {
    #[cfg(test)]
    pub(super) fn for_spend_test(base_url: &str, key: String) -> Result<Self, String> {
        let client = Self::new(base_url, key)?;
        *client.scope.try_lock().expect("cache de test neuf") = Cache::test_user(&client.api_key);
        Ok(client)
    }
    pub(super) async fn seed_scope(&self, user_id: Option<String>) {
        let mut cache = self.scope.lock().await;
        cache.version += 1;
        cache.seed_version = cache.version;
        cache.state = State::Ready(Scope::new(user_id, &self.api_key));
    }

    pub(super) async fn resolve_scope(&self, force: bool) -> Result<Scope, String> {
        let (version, lookup) = {
            let mut cache = self.scope.lock().await;
            match &cache.state {
                State::Ready(scope) => return Ok(scope.clone()),
                State::Loading(version, future) if !force => (*version, future.clone()),
                _ => {
                    // Le futur ne capture pas le cache dans lequel il est stocké.
                    let transport =
                        Self::with_http(&self.base_url, self.api_key.clone(), self.http.clone())?;
                    let future = async move {
                        #[derive(serde::Deserialize)]
                        struct Info {
                            info: User,
                        }
                        #[derive(serde::Deserialize)]
                        struct User {
                            user_id: Option<String>,
                        }
                        let info: Info = tokio::time::timeout(
                            Duration::from_secs(3),
                            transport.get("/key/info"),
                        )
                        .await
                        .map_err(|_| "Identification expirée".to_owned())?
                        .map_err(String::from)?;
                        Ok(Scope::new(info.info.user_id, &transport.api_key))
                    }
                    .boxed()
                    .shared();
                    cache.version += 1;
                    let version = cache.version;
                    cache.state = State::Loading(version, future.clone());
                    (version, future)
                }
            }
        };
        let result = lookup.await;
        let mut cache = self.scope.lock().await;
        if cache.version != version {
            return match (&cache.state, result) {
                (State::Ready(scope), _) if cache.seed_version == cache.version => {
                    Ok(scope.clone())
                }
                // Les appels de même portée gardent leur résultat sans écraser le cache.
                (_, Ok(old)) => Ok(old),
                (_, Err(_)) => Err("Identification remplacée".into()),
            };
        }
        if matches!(cache.state, State::Loading(..)) {
            cache.state = match &result {
                Ok(scope) => State::Ready(scope.clone()),
                Err(_) => State::Unknown,
            };
        }
        result
    }

    pub(super) async fn scoped_path(&self, path: String) -> Result<String, String> {
        Ok(self.resolve_scope(false).await?.append_key(path))
    }
}
