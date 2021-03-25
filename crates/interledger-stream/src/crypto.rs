use bytes::BytesMut;
#[cfg(test)]
use once_cell::sync::Lazy;
use ring::rand::{SecureRandom, SystemRandom};
use ring::{aead, digest, hmac};
use tracing::error;

const NONCE_LENGTH: usize = 12;
const AUTH_TAG_LENGTH: usize = 16;

/// Protocol specific string for encryption
static ENCRYPTION_KEY_STRING: &[u8] = b"ilp_stream_encryption";
/// Protocol specific string for generating fulfillments
static FULFILLMENT_GENERATION_STRING: &[u8] = b"ilp_stream_fulfillment";

/// Returns the HMAC-SHA256 of the provided message using the provided **secret** key
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let output = hmac::sign(&key, message);
    let mut to_return: [u8; 32] = [0; 32];
    to_return.copy_from_slice(output.as_ref());
    to_return
}

/// The fulfillment is generated by HMAC-256'ing the data with a secret key.
/// The secret key is generated deterministically by HMAC-256'ing the shared secret
/// and the hardcoded string "ilp_stream_fulfillment"
pub fn generate_fulfillment(shared_secret: &[u8], data: &[u8]) -> [u8; 32] {
    // generate the key as defined in the specificatoin
    let key = hmac_sha256(shared_secret, &FULFILLMENT_GENERATION_STRING);
    // return the hmac-sha256 of the data based on the generated key
    hmac_sha256(&key[..], data)
}

/// Returns a 32-byte sha256 digest of the provided preimage
pub fn hash_sha256(preimage: &[u8]) -> [u8; 32] {
    let output = digest::digest(&digest::SHA256, preimage);
    let mut to_return: [u8; 32] = [0; 32];
    to_return.copy_from_slice(output.as_ref());
    to_return
}

/// The fulfillment condition is the 32-byte sha256 of the fulfillment
/// generated by the provided shared secret and data via the
/// [generate_fulfillment](./fn.generate_fulfillment.html) function
pub fn generate_condition(shared_secret: &[u8], data: &[u8]) -> [u8; 32] {
    let fulfillment = generate_fulfillment(&shared_secret, &data);
    hash_sha256(&fulfillment)
}

/// Returns a random 32 byte number using [SystemRandom::new()](../../ring/rand/struct.SystemRandom.html#method.new)
pub fn random_condition() -> [u8; 32] {
    let mut condition_slice: [u8; 32] = [0; 32];
    SystemRandom::new()
        .fill(&mut condition_slice)
        .expect("Failed to securely generate random condition!");
    condition_slice
}

/// Returns a random 18 byte number using
/// [SystemRandom::new()](../../ring/rand/struct.SystemRandom.html#method.new)
pub fn generate_token() -> [u8; 18] {
    let mut token: [u8; 18] = [0; 18];
    SystemRandom::new()
        .fill(&mut token)
        .expect("Failed to securely generate a random token!");
    token
}

/// Encrypts a plaintext by calling [encrypt_with_nonce](./fn.encrypt_with_nonce.html)
/// with a random nonce of [`NONCE_LENGTH`](./constant.NONCE_LENGTH.html) generated using
/// [SystemRandom::new()](../../ring/rand/struct.SystemRandom.html#method.new)
pub fn encrypt(shared_secret: &[u8], plaintext: BytesMut) -> BytesMut {
    // Generate a random nonce or IV
    let mut nonce: [u8; NONCE_LENGTH] = [0; NONCE_LENGTH];
    SystemRandom::new()
        .fill(&mut nonce[..])
        .expect("Failed to securely generate a random nonce!");

    encrypt_with_nonce(shared_secret, plaintext, nonce)
}

/// Encrypts a plaintext with a nonce by using AES256-GCM.
///
/// A secret key is generated deterministically by HMAC-256'ing the `shared_secret`
/// and the hardcoded string "ilp_stream_encryption"
///
/// The `additional_data` field is left empty.
///
/// The ciphertext can be decrypted by calling the [`decrypt`](./fn.decrypt.html) function with the
/// same `shared_secret`.
fn encrypt_with_nonce(
    shared_secret: &[u8],
    mut plaintext: BytesMut,
    nonce: [u8; NONCE_LENGTH],
) -> BytesMut {
    let key = hmac_sha256(shared_secret, &ENCRYPTION_KEY_STRING);
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .expect("Failed to create a new sealing key for encrypting data!");
    let key = aead::LessSafeKey::new(key);

    let additional_data = aead::Aad::from(&[]);

    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        additional_data,
        &mut plaintext,
    )
    .unwrap_or_else(|err| {
        error!("Error encrypting {:?}", err);
        panic!("Error encrypting {:?}", err);
    });

    // Rearrange the bytes so that the tag goes first (should have put it last in the JS implementation, but oh well)
    let auth_tag_position = plaintext.len() - AUTH_TAG_LENGTH;
    let mut tag_data = plaintext.split_off(auth_tag_position);
    tag_data.unsplit(plaintext);

    // The format is `nonce, auth tag, data`, in that order
    let mut nonce_tag_data = BytesMut::from(&nonce[..]);
    nonce_tag_data.unsplit(tag_data);

    nonce_tag_data
}

/// Decrypts a AES256-GCM encrypted ciphertext.
///
/// The secret key is generated deterministically by HMAC-256'ing the `shared_secret`
/// and the hardcoded string "ilp_stream_encryption"
///
/// The `additional_data` field is left empty.
///
/// The nonce and auth tag are extracted from the first 12 and 16 bytes
/// of the ciphertext.
pub fn decrypt(shared_secret: &[u8], mut ciphertext: BytesMut) -> Result<BytesMut, ()> {
    use bytes::Buf;

    // FIXME: note the next comment which includes nonce and tag but only makes sure that one of
    // AUTH_TAG_LENGTH is present. This was implemented with bytes04 originally which didn't error
    // for trying to split_to contents which didn't exist. When upgrading to bytes05 this became
    // obvious because split_to now panics.

    // ciphertext must include at least a nonce and tag
    if ciphertext.len() < AUTH_TAG_LENGTH {
        return Err(());
    }
    let key = hmac_sha256(shared_secret, &ENCRYPTION_KEY_STRING);
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .expect("Failed to create a new opening key for decrypting data!");
    let key = aead::LessSafeKey::new(key);

    let mut nonce: [u8; NONCE_LENGTH] = [0; NONCE_LENGTH];
    nonce.copy_from_slice(&ciphertext.split_to(NONCE_LENGTH));

    let additional_data: &[u8] = &[];

    // FIXME: see reason for AUTH_TAG_LENGTH.min(...) from above; at least in many of this crates
    // tests this is empty slice.
    let auth_tag = ciphertext.split_to(AUTH_TAG_LENGTH.min(ciphertext.remaining()));

    // Ring expects the tag to come after the data
    ciphertext.unsplit(auth_tag);

    let length = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(additional_data),
            &mut ciphertext,
        )
        .map_err(|err| {
            // FIXME: many of the tests see this if you have logging on.
            error!("Error decrypting {:?}", err);
        })?
        .len();
    ciphertext.truncate(length);
    Ok(ciphertext)
}

#[cfg(test)]
mod fulfillment_and_condition {
    use super::*;
    use bytes::Bytes;

    static SHARED_SECRET: Lazy<Vec<u8>> = Lazy::new(|| {
        vec![
            126, 219, 117, 93, 118, 248, 249, 211, 20, 211, 65, 110, 237, 80, 253, 179, 81, 146,
            229, 67, 231, 49, 92, 127, 254, 230, 144, 102, 103, 166, 150, 36,
        ]
    });
    static DATA: Lazy<Vec<u8>> = Lazy::new(|| {
        vec![
            119, 248, 213, 234, 63, 200, 224, 140, 212, 222, 105, 159, 246, 203, 66, 155, 151, 172,
            68, 24, 76, 232, 90, 10, 237, 146, 189, 73, 248, 196, 177, 108, 115, 223,
        ]
    });
    static FULFILLMENT: Lazy<Vec<u8>> = Lazy::new(|| {
        vec![
            24, 6, 56, 73, 229, 236, 88, 227, 82, 112, 152, 49, 152, 73, 182, 183, 198, 7, 233,
            124, 119, 65, 13, 68, 54, 108, 120, 193, 59, 226, 107, 39,
        ]
    });

    #[test]
    fn it_generates_the_same_fulfillment_as_javascript() {
        let fulfillment =
            generate_fulfillment(&Bytes::from(&SHARED_SECRET[..]), &Bytes::from(&DATA[..]));
        assert_eq!(fulfillment.to_vec(), *FULFILLMENT);
    }
}

#[cfg(test)]
mod encrypt_decrypt_test {
    use super::*;

    static SHARED_SECRET: &[u8] = &[
        126, 219, 117, 93, 118, 248, 249, 211, 20, 211, 65, 110, 237, 80, 253, 179, 81, 146, 229,
        67, 231, 49, 92, 127, 254, 230, 144, 102, 103, 166, 150, 36,
    ];
    static PLAINTEXT: &[u8] = &[99, 0, 12, 255, 77, 31];
    static CIPHERTEXT: &[u8] = &[
        119, 248, 213, 234, 63, 200, 224, 140, 212, 222, 105, 159, 246, 203, 66, 155, 151, 172, 68,
        24, 76, 232, 90, 10, 237, 146, 189, 73, 248, 196, 177, 108, 115, 223,
    ];
    static NONCE: [u8; NONCE_LENGTH] = [119, 248, 213, 234, 63, 200, 224, 140, 212, 222, 105, 159];

    #[test]
    fn it_encrypts_to_same_as_javascript() {
        let encrypted = encrypt_with_nonce(SHARED_SECRET, BytesMut::from(PLAINTEXT), NONCE);
        assert_eq!(&encrypted[..], CIPHERTEXT);
    }

    #[test]
    fn it_decrypts_javascript_ciphertext() {
        let decrypted = decrypt(SHARED_SECRET, BytesMut::from(CIPHERTEXT));
        assert_eq!(&decrypted.unwrap()[..], PLAINTEXT);
    }

    #[test]
    fn it_losslessly_encrypts_and_decrypts() {
        let ciphertext = encrypt(SHARED_SECRET, BytesMut::from(PLAINTEXT));
        let decrypted = decrypt(SHARED_SECRET, ciphertext);
        assert_eq!(&decrypted.unwrap()[..], PLAINTEXT);
    }
}
