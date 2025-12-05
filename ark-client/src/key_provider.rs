use crate::error::Error;
use bitcoin::bip32::DerivationPath;
use bitcoin::bip32::Xpriv;
use bitcoin::key::Keypair;
use bitcoin::secp256k1::Secp256k1;
use std::sync::Arc;

/// Provides keypairs for signing operations
///
/// This trait allows different key management strategies:
/// - Static keypair (single key)
/// - BIP32 HD wallet (hierarchical deterministic)
/// - Hardware wallets (future)
/// - Custom key derivation schemes
pub trait KeyProvider: Send + Sync {
    /// Get the next keypair for receiving funds
    ///
    /// For static key providers, this always returns the same keypair.
    /// For HD wallets, this should derive and return the next unused keypair.
    ///
    /// # Returns
    ///
    /// A keypair to use for the next receiving address
    fn get_next_keypair(&self) -> Result<Keypair, Error>;

    /// Get a keypair for a specific BIP32 derivation path
    ///
    /// # Arguments
    ///
    /// * `path` - BIP32 derivation path as an array of child indexes
    ///
    /// # Returns
    ///
    /// A keypair derived at the specified path, or an error if derivation is not supported
    fn get_keypair_for_path(&self, path: &[u32]) -> Result<Keypair, Error>;

    /// Get a keypair for a specific public key
    ///
    /// This is essential for HD wallets where you need to find the correct keypair
    /// for signing with a previously generated public key.
    ///
    /// # Arguments
    ///
    /// * `pk` - The X-only public key to find the keypair for
    ///
    /// # Returns
    ///
    /// The keypair corresponding to the public key, or an error if not found
    fn get_keypair_for_pk(&self, pk: &bitcoin::XOnlyPublicKey) -> Result<Keypair, Error>;

    /// Get all public keys that this provider currently knows about
    ///
    /// For static key providers, this returns the single keypair's public key.
    /// For HD wallets, this returns all public keys that have been derived and cached
    /// (i.e., keys generated via `get_next_keypair`).
    ///
    /// This is useful for determining which keys are available for signing operations
    /// without having to search or derive new keys.
    ///
    /// # Returns
    ///
    /// A vector of X-only public keys known to this provider
    fn get_cached_pks(&self) -> Result<Vec<bitcoin::XOnlyPublicKey>, Error>;
}

/// A simple key provider that uses a static keypair
///
/// This is the simplest implementation and is backward compatible with
/// the original single-keypair design.
#[derive(Clone)]
pub struct StaticKeyProvider {
    kp: Keypair,
}

impl StaticKeyProvider {
    /// Create a new static key provider
    pub fn new(kp: Keypair) -> Self {
        Self { kp }
    }
}

impl KeyProvider for StaticKeyProvider {
    fn get_next_keypair(&self) -> Result<Keypair, Error> {
        // Static provider always returns the same keypair
        Ok(self.kp)
    }

    fn get_keypair_for_path(&self, _path: &[u32]) -> Result<Keypair, Error> {
        // Static provider always returns the same keypair
        Ok(self.kp)
    }

    fn get_keypair_for_pk(&self, pk: &bitcoin::XOnlyPublicKey) -> Result<Keypair, Error> {
        // Verify that the requested public key matches our keypair
        let our_pk = self.kp.x_only_public_key().0;
        if &our_pk == pk {
            Ok(self.kp)
        } else {
            Err(Error::ad_hoc(format!(
                "Public key mismatch: requested {pk}, but only have {our_pk}"
            )))
        }
    }

    fn get_cached_pks(&self) -> Result<Vec<bitcoin::XOnlyPublicKey>, Error> {
        Ok(vec![self.kp.public_key().into()])
    }
}

/// A BIP32 hierarchical deterministic key provider
///
/// This provider derives keypairs from a master extended private key
/// using BIP32 derivation paths. It maintains an index counter for
/// generating new receiving addresses.
///
/// ## Example
///
/// ```rust
/// # use std::str::FromStr;
/// # use bitcoin::bip32::{Xpriv, DerivationPath};
/// # use bitcoin::Network;
/// # use ark_client::Bip32KeyProvider;
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Create from a master key with a base path (e.g., m/84'/0'/0'/0)
/// let master_key = Xpriv::from_str("xprv...")?;
/// let base_path = DerivationPath::from_str("m/84'/0'/0'/0")?;
///
/// // This will derive keys at m/84'/0'/0'/0/0, m/84'/0'/0'/0/1, etc.
/// let provider = Bip32KeyProvider::new(master_key, base_path);
///
/// // Get the next receiving keypair (increments index)
/// let kp1 = provider.get_next_keypair()?; // m/84'/0'/0'/0/0
/// let kp2 = provider.get_next_keypair()?; // m/84'/0'/0'/0/1
///
/// // Or derive a specific keypair by path
/// let custom_path = vec![84 + 0x8000_0000, 0x8000_0000, 0x8000_0000, 0, 5];
/// let kp = provider.get_keypair_for_path(&custom_path)?;
/// # Ok(())
/// # }
/// ```
pub struct Bip32KeyProvider {
    master_key: Xpriv,
    base_path: DerivationPath,
    // Using std::sync::Mutex for interior mutability across Send + Sync
    next_index: Arc<std::sync::Mutex<u32>>,
    // Cache of derived keys: pk -> (path_index, keypair)
    key_cache:
        Arc<std::sync::RwLock<std::collections::HashMap<bitcoin::XOnlyPublicKey, (u32, Keypair)>>>,
}

impl Bip32KeyProvider {
    /// Create a new BIP32 key provider
    ///
    /// # Arguments
    ///
    /// * `master_key` - The master extended private key (xpriv)
    /// * `base_path` - The base derivation path (e.g., m/84'/0'/0'/0). The provider will append
    ///   index numbers to this path.
    pub fn new(master_key: Xpriv, base_path: DerivationPath) -> Self {
        Self {
            master_key,
            base_path,
            next_index: Arc::new(std::sync::Mutex::new(0)),
            key_cache: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Create a new BIP32 key provider starting from a specific index
    ///
    /// # Arguments
    ///
    /// * `master_key` - The master extended private key (xpriv)
    /// * `base_path` - The base derivation path
    /// * `start_index` - The starting index for key derivation
    pub fn new_with_index(master_key: Xpriv, base_path: DerivationPath, start_index: u32) -> Self {
        Self {
            master_key,
            base_path,
            next_index: Arc::new(std::sync::Mutex::new(start_index)),
            key_cache: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Derive a keypair at the specified path
    fn derive_keypair(&self, path: &DerivationPath) -> Result<Keypair, Error> {
        let secp = Secp256k1::new();
        let derived_key = self
            .master_key
            .derive_priv(&secp, path)
            .map_err(|e| Error::ad_hoc(format!("BIP32 derivation failed: {e}")))?;

        Ok(derived_key.to_keypair(&secp))
    }

    /// Derive a keypair at base_path/index
    fn derive_at_index(&self, index: u32) -> Result<Keypair, Error> {
        use bitcoin::bip32::ChildNumber;

        let path = self.base_path.clone();
        let path = path.extend([ChildNumber::Normal { index }]);

        self.derive_keypair(&path)
    }
}

impl KeyProvider for Bip32KeyProvider {
    fn get_next_keypair(&self) -> Result<Keypair, Error> {
        // Get and increment the next index
        let index = {
            let mut next_index = self
                .next_index
                .lock()
                .map_err(|e| Error::ad_hoc(format!("Failed to lock next_index: {e}")))?;
            let current = *next_index;
            *next_index = next_index
                .checked_add(1)
                .ok_or_else(|| Error::ad_hoc("Key derivation index overflow"))?;
            current
        };

        // Derive the keypair at this index
        let kp = self.derive_at_index(index)?;

        // Cache it for later lookup
        let pk = kp.x_only_public_key().0;
        {
            let mut cache = self
                .key_cache
                .write()
                .map_err(|e| Error::ad_hoc(format!("Failed to lock key_cache: {e}")))?;
            cache.insert(pk, (index, kp));
        }

        Ok(kp)
    }

    fn get_keypair_for_path(&self, path: &[u32]) -> Result<Keypair, Error> {
        use bitcoin::bip32::ChildNumber;
        let child_numbers: Vec<ChildNumber> = path
            .iter()
            .map(|&n| {
                if n & 0x8000_0000 != 0 {
                    ChildNumber::Hardened {
                        index: n & 0x7FFF_FFFF,
                    }
                } else {
                    ChildNumber::Normal { index: n }
                }
            })
            .collect();
        let derivation_path = DerivationPath::from(child_numbers);
        self.derive_keypair(&derivation_path)
    }

    fn get_keypair_for_pk(&self, pk: &bitcoin::XOnlyPublicKey) -> Result<Keypair, Error> {
        // First check the cache
        {
            let cache = self
                .key_cache
                .read()
                .map_err(|e| Error::ad_hoc(format!("Failed to lock key_cache: {e}")))?;
            if let Some((_, kp)) = cache.get(pk) {
                return Ok(*kp);
            }
        }

        // If not in cache, we need to search. For now, we'll search up to the current index
        let current_index = {
            let next_index = self
                .next_index
                .lock()
                .map_err(|e| Error::ad_hoc(format!("Failed to lock next_index: {e}")))?;
            *next_index
        };

        // Search through derived keys up to current index
        for i in 0..current_index {
            let kp = self.derive_at_index(i)?;
            let derived_pk = kp.x_only_public_key().0;

            if &derived_pk == pk {
                // Cache it for next time
                let mut cache = self
                    .key_cache
                    .write()
                    .map_err(|e| Error::ad_hoc(format!("Failed to lock key_cache: {e}")))?;
                cache.insert(derived_pk, (i, kp));
                return Ok(kp);
            }
        }

        Err(Error::ad_hoc(format!(
            "Public key {pk} not found in HD wallet. \
            Searched indices 0..{current_index}. \
            The key may have been generated outside this provider."
        )))
    }

    fn get_cached_pks(&self) -> Result<Vec<bitcoin::XOnlyPublicKey>, Error> {
        let cache = self
            .key_cache
            .read()
            .map_err(|e| Error::ad_hoc(format!("Failed to lock key_cache: {e}")))?;

        Ok(cache.keys().copied().collect())
    }
}

// Implement KeyProvider for Arc<T> where T: KeyProvider
impl<T: KeyProvider> KeyProvider for Arc<T> {
    fn get_next_keypair(&self) -> Result<Keypair, Error> {
        (**self).get_next_keypair()
    }

    fn get_keypair_for_path(&self, path: &[u32]) -> Result<Keypair, Error> {
        (**self).get_keypair_for_path(path)
    }

    fn get_keypair_for_pk(&self, pk: &bitcoin::XOnlyPublicKey) -> Result<Keypair, Error> {
        (**self).get_keypair_for_pk(pk)
    }

    fn get_cached_pks(&self) -> Result<Vec<bitcoin::XOnlyPublicKey>, Error> {
        (**self).get_cached_pks()
    }
}
