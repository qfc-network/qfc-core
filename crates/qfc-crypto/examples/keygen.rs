//! Generate Ed25519 validator keypairs with derived QFC addresses.
//!
//! Usage: cargo run --example keygen -p qfc-crypto

use qfc_crypto::{address_from_public_key, Keypair};

fn main() {
    println!("=== QFC Validator Key Generation ===\n");

    for i in 1..=3 {
        let keypair = Keypair::generate();
        let secret = keypair.secret_bytes();
        let pubkey = keypair.public_key();
        let address = address_from_public_key(&pubkey);

        println!("Validator {}:", i);
        println!("  Secret Key : {}", hex::encode(secret));
        println!("  Public Key : {}", hex::encode(pubkey.as_bytes()));
        println!("  Address    : 0x{}", hex::encode(address.as_bytes()));
        println!();
    }
}
