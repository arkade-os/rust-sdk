//! FROST 2-of-3 key generation and signing for the sample.
//!
//! Every multi-party exchange happens by copy-pasting base64 blobs between the three actor
//! terminals. The protocol itself is implemented as pure blob-in/blob-out steps ([`dkg_start`],
//! [`DkgRound1`], [`DkgRound2`], [`FrostActor::start_signing`], [`FrostActor::respond`],
//! [`FrostActor::finalize_signing`]); the interactive prompts are thin wrappers around them, and
//! the end-to-end test drives the same steps in a single process.
//!
//! Secrets (DKG round secrets, signing nonces) only ever live in memory of a single command
//! invocation; the produced key share and group key are persisted per actor.

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use bitcoin::hex::DisplayHex;
use bitcoin::hex::FromHex;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::XOnlyPublicKey;
use frost_secp256k1_tr as frost;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::BufRead;
use std::io::Write;
use std::path::Path;

pub const ACTORS: [&str; 3] = ["alice", "bob", "clair"];

const MIN_SIGNERS: u16 = 2;
const MAX_SIGNERS: u16 = 3;

const KEY_PACKAGE_FILE: &str = "frost_key_package.hex";
const PUBKEY_PACKAGE_FILE: &str = "frost_pubkey_package.hex";

pub fn actor_identifier(actor: &str) -> Result<frost::Identifier> {
    let index = ACTORS
        .iter()
        .position(|a| *a == actor)
        .ok_or_else(|| anyhow!("unknown actor '{actor}', expected one of {ACTORS:?}"))?;

    frost::Identifier::try_from(index as u16 + 1).map_err(|e| anyhow!("invalid identifier: {e}"))
}

fn actor_by_identifier(identifier: &frost::Identifier) -> Result<&'static str> {
    for actor in ACTORS {
        if actor_identifier(actor)? == *identifier {
            return Ok(actor);
        }
    }
    bail!("no actor for identifier");
}

fn others(actor: &str) -> Vec<&'static str> {
    ACTORS.iter().copied().filter(|a| *a != actor).collect()
}

/// A message passed between actor terminals, as a single-line base64 blob.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Payload {
    DkgRound1 {
        from: String,
        package: String,
    },
    DkgRound2 {
        from: String,
        to: String,
        package: String,
    },
    SignRequest {
        from: String,
        msg: String,
        commitments: String,
    },
    SignResponse {
        from: String,
        commitments: String,
        sig_share: String,
    },
}

fn encode(payload: &Payload) -> Result<String> {
    Ok(BASE64.encode(serde_json::to_vec(payload)?))
}

fn decode(blob: &str) -> Result<Payload> {
    let bytes = BASE64
        .decode(blob.trim())
        .context("blob is not valid base64")?;
    serde_json::from_slice(&bytes).context("blob does not contain a valid payload")
}

fn prompt(text: &str) -> Result<String> {
    eprint!("{text}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("failed to read from stdin")?;
    let line = line.trim().to_string();
    if line.is_empty() {
        bail!("empty input");
    }
    Ok(line)
}

// === Distributed key generation ===

/// DKG state after producing this actor's round 1 package.
pub struct DkgRound1 {
    actor: String,
    secret: frost::keys::dkg::round1::SecretPackage,
    blob: String,
}

/// Start the 2-of-3 DKG for `actor`, producing the round 1 blob to broadcast to both peers.
pub fn dkg_start(actor: &str) -> Result<DkgRound1> {
    let identifier = actor_identifier(actor)?;
    let mut rng = rand::thread_rng();

    let (secret, package) = frost::keys::dkg::part1(identifier, MAX_SIGNERS, MIN_SIGNERS, &mut rng)
        .map_err(|e| anyhow!("DKG part 1 failed: {e}"))?;

    let blob = encode(&Payload::DkgRound1 {
        from: actor.to_string(),
        package: package
            .serialize()
            .map_err(|e| anyhow!("{e}"))?
            .to_lower_hex_string(),
    })?;

    Ok(DkgRound1 {
        actor: actor.to_string(),
        secret,
        blob,
    })
}

impl DkgRound1 {
    /// The round 1 blob to send to BOTH other actors.
    pub fn blob(&self) -> &str {
        &self.blob
    }

    /// Consume the round 1 blobs of both peers (any order) and produce the per-peer round 2
    /// blobs.
    pub fn receive_round1(self, blobs: &[&str]) -> Result<DkgRound2> {
        let mut round1_packages = BTreeMap::new();
        for blob in blobs {
            let Payload::DkgRound1 { from, package } = decode(blob)? else {
                bail!("expected a round 1 blob");
            };
            if from == self.actor {
                bail!("received my own round 1 blob back");
            }
            let package = frost::keys::dkg::round1::Package::deserialize(&Vec::from_hex(&package)?)
                .map_err(|e| anyhow!("invalid round 1 package: {e}"))?;
            if round1_packages
                .insert(actor_identifier(&from)?, package)
                .is_some()
            {
                bail!("received two round 1 blobs from '{from}'");
            }
        }
        if round1_packages.len() != others(&self.actor).len() {
            bail!("expected round 1 blobs from {:?}", others(&self.actor));
        }

        let (secret, round2_packages) = frost::keys::dkg::part2(self.secret, &round1_packages)
            .map_err(|e| anyhow!("DKG part 2 failed: {e}"))?;

        let mut outgoing = Vec::new();
        for (peer_identifier, package) in round2_packages {
            let peer = actor_by_identifier(&peer_identifier)?;
            let blob = encode(&Payload::DkgRound2 {
                from: self.actor.clone(),
                to: peer.to_string(),
                package: package
                    .serialize()
                    .map_err(|e| anyhow!("{e}"))?
                    .to_lower_hex_string(),
            })?;
            outgoing.push((peer.to_string(), blob));
        }

        Ok(DkgRound2 {
            actor: self.actor,
            round1_packages,
            secret,
            outgoing,
        })
    }
}

/// DKG state after producing the per-peer round 2 packages.
pub struct DkgRound2 {
    actor: String,
    round1_packages: BTreeMap<frost::Identifier, frost::keys::dkg::round1::Package>,
    secret: frost::keys::dkg::round2::SecretPackage,
    outgoing: Vec<(String, String)>,
}

impl DkgRound2 {
    /// The round 2 blobs, as `(recipient, blob)` pairs. Each blob must go to exactly the named
    /// recipient (and nobody else).
    pub fn outgoing(&self) -> &[(String, String)] {
        &self.outgoing
    }

    /// Consume the round 2 blobs both peers addressed to this actor (any order) and derive the
    /// key share and group key.
    pub fn receive_round2(self, blobs: &[&str]) -> Result<FrostActor> {
        let mut round2_packages = BTreeMap::new();
        for blob in blobs {
            let Payload::DkgRound2 { from, to, package } = decode(blob)? else {
                bail!("expected a round 2 blob");
            };
            if from == self.actor {
                bail!("received my own round 2 blob back");
            }
            if to != self.actor {
                bail!(
                    "this round 2 blob is addressed to '{to}', not to '{}'",
                    self.actor
                );
            }
            let package = frost::keys::dkg::round2::Package::deserialize(&Vec::from_hex(&package)?)
                .map_err(|e| anyhow!("invalid round 2 package: {e}"))?;
            if round2_packages
                .insert(actor_identifier(&from)?, package)
                .is_some()
            {
                bail!("received two round 2 blobs from '{from}'");
            }
        }
        if round2_packages.len() != others(&self.actor).len() {
            bail!("expected round 2 blobs from {:?}", others(&self.actor));
        }

        let (key_package, pubkey_package) =
            frost::keys::dkg::part3(&self.secret, &self.round1_packages, &round2_packages)
                .map_err(|e| anyhow!("DKG part 3 failed: {e}"))?;

        FrostActor::new(&self.actor, key_package, pubkey_package)
    }
}

/// Run the interactive 2-of-3 distributed key generation for `actor`.
///
/// All three actors must run this concurrently in separate terminals and exchange the printed
/// blobs. On success the actor's key share and the group public key package are written to `dir`.
pub fn run_dkg(actor: &str, dir: &Path) -> Result<XOnlyPublicKey> {
    eprintln!("Running FROST 2-of-3 DKG as '{actor}'.");
    eprintln!("All three actors must run `keygen` at the same time.\n");

    let round1 = dkg_start(actor)?;

    eprintln!("Send this round 1 blob to BOTH other actors:\n");
    println!("{}\n", round1.blob());

    let mut round1_blobs = Vec::new();
    for other in others(actor) {
        round1_blobs.push(prompt(&format!("Paste {other}'s round 1 blob: "))?);
    }
    let round2 =
        round1.receive_round1(&round1_blobs.iter().map(String::as_str).collect::<Vec<_>>())?;

    eprintln!();
    for (peer, blob) in round2.outgoing() {
        eprintln!("Send this round 2 blob to {peer} (and ONLY {peer}):\n");
        println!("{blob}\n");
    }

    let mut round2_blobs = Vec::new();
    for other in others(actor) {
        round2_blobs.push(prompt(&format!(
            "Paste the round 2 blob {other} made for you: "
        ))?);
    }
    let frost_actor =
        round2.receive_round2(&round2_blobs.iter().map(String::as_str).collect::<Vec<_>>())?;

    frost_actor.save(dir)?;

    let group_pk = frost_actor.group_pk();
    eprintln!("\nDKG complete. Group public key (x-only): {group_pk}");
    eprintln!(
        "Key share written to {}",
        dir.join(KEY_PACKAGE_FILE).display()
    );

    Ok(group_pk)
}

fn group_x_only_pk(pubkey_package: &frost::keys::PublicKeyPackage) -> Result<XOnlyPublicKey> {
    let bytes = pubkey_package
        .verifying_key()
        .serialize()
        .map_err(|e| anyhow!("{e}"))?;
    // 33-byte compressed SEC1 encoding; BIP340 uses the 32-byte x coordinate.
    XOnlyPublicKey::from_slice(&bytes[1..]).context("invalid group public key")
}

/// An actor's share of the FROST group key.
pub struct FrostActor {
    actor: String,
    key_package: frost::keys::KeyPackage,
    pubkey_package: frost::keys::PublicKeyPackage,
    group_pk: XOnlyPublicKey,
}

/// An in-flight signing ceremony on the coordinator side.
///
/// Holds the coordinator's secret nonces; consumed by [`FrostActor::finalize_signing`] so nonces
/// cannot be reused.
pub struct SigningSession {
    nonces: frost::round1::SigningNonces,
    commitments: frost::round1::SigningCommitments,
    msg: [u8; 32],
    request_blob: String,
}

impl SigningSession {
    /// The signing request blob to send to ONE other actor.
    pub fn request_blob(&self) -> &str {
        &self.request_blob
    }
}

/// A participant's answer to a signing request.
pub struct SignResponse {
    /// The actor who started the ceremony.
    pub coordinator: String,
    /// The 32-byte message (sighash) being signed.
    pub msg: [u8; 32],
    /// The response blob to paste back into the coordinator's terminal.
    pub blob: String,
}

impl FrostActor {
    fn new(
        actor: &str,
        key_package: frost::keys::KeyPackage,
        pubkey_package: frost::keys::PublicKeyPackage,
    ) -> Result<Self> {
        let group_pk = group_x_only_pk(&pubkey_package)?;

        Ok(Self {
            actor: actor.to_string(),
            key_package,
            pubkey_package,
            group_pk,
        })
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(
            dir.join(KEY_PACKAGE_FILE),
            self.key_package
                .serialize()
                .map_err(|e| anyhow!("{e}"))?
                .to_lower_hex_string(),
        )?;
        std::fs::write(
            dir.join(PUBKEY_PACKAGE_FILE),
            self.pubkey_package
                .serialize()
                .map_err(|e| anyhow!("{e}"))?
                .to_lower_hex_string(),
        )?;
        Ok(())
    }

    pub fn load(actor: &str, dir: &Path) -> Result<Self> {
        let read_hex = |file: &str| -> Result<Vec<u8>> {
            let path = dir.join(file);
            let hex = std::fs::read_to_string(&path).with_context(|| {
                format!(
                    "failed to read {}; run `keygen` for '{actor}' first",
                    path.display()
                )
            })?;
            Ok(Vec::from_hex(hex.trim())?)
        };

        let key_package = frost::keys::KeyPackage::deserialize(&read_hex(KEY_PACKAGE_FILE)?)
            .map_err(|e| anyhow!("invalid key package: {e}"))?;
        let pubkey_package =
            frost::keys::PublicKeyPackage::deserialize(&read_hex(PUBKEY_PACKAGE_FILE)?)
                .map_err(|e| anyhow!("invalid public key package: {e}"))?;

        Self::new(actor, key_package, pubkey_package)
    }

    pub fn group_pk(&self) -> XOnlyPublicKey {
        self.group_pk
    }

    /// Start a signing ceremony over a 32-byte message, producing the request blob for one other
    /// actor.
    pub fn start_signing(&self, msg: &[u8; 32]) -> Result<SigningSession> {
        let mut rng = rand::thread_rng();

        let (nonces, commitments) =
            frost::round1::commit(self.key_package.signing_share(), &mut rng);

        let request_blob = encode(&Payload::SignRequest {
            from: self.actor.clone(),
            msg: msg.to_lower_hex_string(),
            commitments: commitments
                .serialize()
                .map_err(|e| anyhow!("{e}"))?
                .to_lower_hex_string(),
        })?;

        Ok(SigningSession {
            nonces,
            commitments,
            msg: *msg,
            request_blob,
        })
    }

    /// Answer a signing request from another actor.
    ///
    /// Commitment and signature share are produced in one step: with two participants, the
    /// coordinator's commitments in the request complete the commitment set.
    pub fn respond(&self, request_blob: &str) -> Result<SignResponse> {
        let my_identifier = actor_identifier(&self.actor)?;
        let mut rng = rand::thread_rng();

        let Payload::SignRequest {
            from,
            msg,
            commitments: coordinator_commitments,
        } = decode(request_blob)?
        else {
            bail!("expected a signing request blob");
        };
        if from == self.actor {
            bail!("cannot respond to my own signing request");
        }
        let coordinator_identifier = actor_identifier(&from)?;
        let coordinator_commitments = frost::round1::SigningCommitments::deserialize(
            &Vec::from_hex(&coordinator_commitments)?,
        )
        .map_err(|e| anyhow!("invalid commitments: {e}"))?;

        let msg: [u8; 32] = <[u8; 32]>::from_hex(&msg).context("message must be 32 bytes")?;

        let (nonces, my_commitments) =
            frost::round1::commit(self.key_package.signing_share(), &mut rng);

        let mut commitments_map = BTreeMap::new();
        commitments_map.insert(coordinator_identifier, coordinator_commitments);
        commitments_map.insert(my_identifier, my_commitments);

        let signing_package = frost::SigningPackage::new(commitments_map, &msg);

        let share = frost::round2::sign(&signing_package, &nonces, &self.key_package)
            .map_err(|e| anyhow!("failed to produce signature share: {e}"))?;

        let blob = encode(&Payload::SignResponse {
            from: self.actor.clone(),
            commitments: my_commitments
                .serialize()
                .map_err(|e| anyhow!("{e}"))?
                .to_lower_hex_string(),
            sig_share: share.serialize().to_lower_hex_string(),
        })?;

        Ok(SignResponse {
            coordinator: from,
            msg,
            blob,
        })
    }

    /// Combine the participant's response with the coordinator's own share into a BIP340
    /// signature for the group key.
    pub fn finalize_signing(
        &self,
        session: SigningSession,
        response_blob: &str,
    ) -> Result<schnorr::Signature> {
        let my_identifier = actor_identifier(&self.actor)?;

        let Payload::SignResponse {
            from,
            commitments: their_commitments,
            sig_share,
        } = decode(response_blob)?
        else {
            bail!("expected a signing response blob");
        };
        if from == self.actor {
            bail!("the response must come from a different actor");
        }
        let their_identifier = actor_identifier(&from)?;
        let their_commitments =
            frost::round1::SigningCommitments::deserialize(&Vec::from_hex(&their_commitments)?)
                .map_err(|e| anyhow!("invalid commitments: {e}"))?;
        let their_share = frost::round2::SignatureShare::deserialize(&Vec::from_hex(&sig_share)?)
            .map_err(|e| anyhow!("invalid signature share: {e}"))?;

        let mut commitments_map = BTreeMap::new();
        commitments_map.insert(my_identifier, session.commitments);
        commitments_map.insert(their_identifier, their_commitments);

        let signing_package = frost::SigningPackage::new(commitments_map, &session.msg);

        let my_share = frost::round2::sign(&signing_package, &session.nonces, &self.key_package)
            .map_err(|e| anyhow!("failed to produce signature share: {e}"))?;

        let mut shares = BTreeMap::new();
        shares.insert(my_identifier, my_share);
        shares.insert(their_identifier, their_share);

        let signature = frost::aggregate(&signing_package, &shares, &self.pubkey_package)
            .map_err(|e| anyhow!("failed to aggregate signature shares: {e}"))?;

        let signature = to_bip340_signature(&signature)?;

        // Sanity check against the group key before handing the signature to the client.
        let secp = Secp256k1::new();
        secp.verify_schnorr(
            &signature,
            &secp256k1::Message::from_digest(session.msg),
            &self.group_pk,
        )
        .context("aggregated FROST signature failed BIP340 verification")?;

        Ok(signature)
    }

    /// Coordinate a 2-of-3 signing ceremony for a 32-byte message, interactively.
    ///
    /// Prints a signing request blob, waits for one other actor to respond via `sign`, and
    /// aggregates both signature shares into a BIP340 signature for the group key.
    pub fn coordinate_signature(&self, msg: &[u8; 32]) -> Result<schnorr::Signature> {
        let session = self.start_signing(msg)?;

        eprintln!("\nA signature from the FROST group key is required.");
        eprintln!("Send this signing request to ONE other actor, who must run `sign <blob>`:\n");
        println!("{}\n", session.request_blob());

        let response_blob = prompt("Paste their response: ")?;
        let signature = self.finalize_signing(session, &response_blob)?;

        eprintln!("Signature aggregated and verified against the group key.\n");

        Ok(signature)
    }

    /// Participate in a signing ceremony started by another actor, interactively.
    ///
    /// Consumes a signing request blob and prints the response blob to paste back into the
    /// coordinator's terminal.
    pub fn respond_to_signing_request(&self, blob: &str) -> Result<()> {
        let response = self.respond(blob)?;

        eprintln!("Signing request from '{}'.", response.coordinator);
        eprintln!("Message (sighash): {}", response.msg.to_lower_hex_string());
        eprintln!("\nPaste this response into the coordinator's terminal:\n");
        println!("{}", response.blob);

        Ok(())
    }
}

fn to_bip340_signature(signature: &frost::Signature) -> Result<schnorr::Signature> {
    let bytes = signature.serialize().map_err(|e| anyhow!("{e}"))?;
    // The taproot ciphersuite serializes signatures in 64-byte BIP340 form (R.x || s).
    schnorr::Signature::from_slice(&bytes)
        .context("FROST signature is not a valid BIP340 signature")
}

/// [`ark_client::Signer`] backed by an interactive FROST signing ceremony.
pub struct FrostSigner {
    actor: FrostActor,
}

impl FrostSigner {
    pub fn new(actor: FrostActor) -> Self {
        Self { actor }
    }

    pub fn group_pk(&self) -> XOnlyPublicKey {
        self.actor.group_pk()
    }
}

impl ark_client::Signer for FrostSigner {
    fn signing_pks(&self) -> Result<Vec<XOnlyPublicKey>, ark_client::Error> {
        Ok(vec![self.actor.group_pk()])
    }

    fn sign_schnorr(
        &self,
        pk: &XOnlyPublicKey,
        msg: &secp256k1::Message,
    ) -> Result<schnorr::Signature, ark_client::Error> {
        if *pk != self.actor.group_pk() {
            return Err(ark_client::Error::consumer(anyhow!(
                "cannot sign for {pk}: FROST group key is {}",
                self.actor.group_pk()
            )));
        }

        let mut msg32 = [0u8; 32];
        msg32.copy_from_slice(msg.as_ref());

        self.actor
            .coordinate_signature(&msg32)
            .map_err(ark_client::Error::consumer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::thread_rng;
    use std::collections::HashMap;

    /// Run the full 2-of-3 DKG through the same blob exchange the three terminals would do,
    /// entirely in one process.
    fn run_dkg_via_blobs() -> HashMap<&'static str, FrostActor> {
        // Round 1: every actor broadcasts one blob to both peers.
        let round1: HashMap<&str, DkgRound1> = ACTORS
            .iter()
            .map(|actor| (*actor, dkg_start(actor).expect("dkg start")))
            .collect();
        let round1_blobs: HashMap<&str, String> = round1
            .iter()
            .map(|(actor, state)| (*actor, state.blob().to_string()))
            .collect();

        // Round 2: every actor consumes the peers' round 1 blobs and produces one blob per peer.
        let mut round2 = HashMap::new();
        let mut inboxes: HashMap<&str, Vec<String>> = HashMap::new();
        for (actor, state) in round1 {
            let peer_blobs: Vec<&str> = others(actor)
                .iter()
                .map(|peer| round1_blobs[peer].as_str())
                .collect();
            let state = state.receive_round1(&peer_blobs).expect("dkg round 1");

            for (recipient, blob) in state.outgoing() {
                inboxes
                    .entry(
                        ACTORS
                            .iter()
                            .find(|a| *a == recipient)
                            .expect("known actor"),
                    )
                    .or_default()
                    .push(blob.clone());
            }
            round2.insert(actor, state);
        }

        // Round 3: every actor consumes the round 2 blobs addressed to it.
        round2
            .into_iter()
            .map(|(actor, state)| {
                let inbox: Vec<&str> = inboxes[actor].iter().map(String::as_str).collect();
                (actor, state.receive_round2(&inbox).expect("dkg round 2"))
            })
            .collect()
    }

    /// One-process end-to-end test of the whole sample protocol: DKG between alice, bob and
    /// clair via blobs, then a signing ceremony for every coordinator/responder pair, checking
    /// each aggregated signature is valid BIP340 for the x-only group key (what the Arkade VTXO
    /// script paths verify).
    #[test]
    fn dkg_and_signing_end_to_end_via_blobs() {
        let actors = run_dkg_via_blobs();

        // All actors must agree on the group key.
        let group_pk = actors["alice"].group_pk();
        for actor in ACTORS {
            assert_eq!(actors[actor].group_pk(), group_pk);
        }

        let secp = Secp256k1::new();
        let msg = [7u8; 32];

        // Any actor can coordinate with any other actor responding.
        for coordinator in ACTORS {
            for responder in others(coordinator) {
                let session = actors[coordinator]
                    .start_signing(&msg)
                    .expect("start signing");

                let response = actors[responder]
                    .respond(session.request_blob())
                    .expect("respond");
                assert_eq!(response.coordinator, coordinator);
                assert_eq!(response.msg, msg);

                let signature = actors[coordinator]
                    .finalize_signing(session, &response.blob)
                    .expect("finalize");

                secp.verify_schnorr(&signature, &secp256k1::Message::from_digest(msg), &group_pk)
                    .expect("valid BIP340 signature for the x-only group key");
            }
        }
    }

    #[test]
    fn responding_to_own_request_fails() {
        let actors = run_dkg_via_blobs();

        let session = actors["alice"]
            .start_signing(&[1u8; 32])
            .expect("start signing");

        assert!(actors["alice"].respond(session.request_blob()).is_err());
    }

    /// A 2-of-3 FROST signature must verify as a plain BIP340 signature for the x-only group
    /// key, which is what the Arkade VTXO script paths check.
    #[test]
    fn frost_signature_is_valid_bip340() {
        let mut rng = thread_rng();

        let (shares, pubkey_package) = frost::keys::generate_with_dealer(
            MAX_SIGNERS,
            MIN_SIGNERS,
            frost::keys::IdentifierList::Default,
            &mut rng,
        )
        .expect("dealer keygen");

        let key_packages: BTreeMap<_, _> = shares
            .into_iter()
            .map(|(identifier, share)| {
                (
                    identifier,
                    frost::keys::KeyPackage::try_from(share).expect("key package"),
                )
            })
            .collect();

        let msg = [42u8; 32];

        // Any two of the three participants sign.
        let signers: Vec<_> = key_packages.iter().take(2).collect();

        let mut nonces_map = BTreeMap::new();
        let mut commitments_map = BTreeMap::new();
        for (identifier, key_package) in &signers {
            let (nonces, commitments) =
                frost::round1::commit(key_package.signing_share(), &mut rng);
            nonces_map.insert(**identifier, nonces);
            commitments_map.insert(**identifier, commitments);
        }

        let signing_package = frost::SigningPackage::new(commitments_map, &msg);

        let mut shares_map = BTreeMap::new();
        for (identifier, key_package) in &signers {
            let share = frost::round2::sign(&signing_package, &nonces_map[identifier], key_package)
                .expect("signature share");
            shares_map.insert(**identifier, share);
        }

        let signature =
            frost::aggregate(&signing_package, &shares_map, &pubkey_package).expect("aggregate");

        let signature = to_bip340_signature(&signature).expect("BIP340 signature");
        let group_pk = group_x_only_pk(&pubkey_package).expect("group pk");

        Secp256k1::new()
            .verify_schnorr(&signature, &secp256k1::Message::from_digest(msg), &group_pk)
            .expect("valid BIP340 signature for the x-only group key");
    }
}
