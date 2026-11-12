use crate::bignum::{MontgomeryModulus, limbs_from_be_bytes, limbs_to_be_bytes};
use crate::sha256::sha256;
use std::sync::OnceLock;

const GROUP_PRIME_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AACAA68FFFFFFFFFFFFFFFF";
const HASH_DOMAIN: &[u8] = b"eggdb-psi-v1";
const DIGEST_BYTES: usize = 32;

/// The length of a blinded element: one element of RFC 3526's 2048-bit MODP group.
pub const PSI_ELEMENT_BYTES: usize = 256;
/// The fewest bytes a private set intersection key may have.
pub const PSI_MIN_KEY_BYTES: usize = 32;
/// The most bytes a private set intersection key may have.
pub const PSI_MAX_KEY_BYTES: usize = 64;

/// Why an element could not be blinded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsiError {
    /// The key is shorter than [`PSI_MIN_KEY_BYTES`], longer than [`PSI_MAX_KEY_BYTES`] or all
    /// zero bytes.
    WeakKey,
    /// The blinded element is not a member of the prime-order subgroup.
    NotAnElement,
}

struct Group {
    modulus: MontgomeryModulus,
    order: Vec<u64>,
}

fn group() -> Option<&'static Group> {
    static GROUP: OnceLock<Option<Group>> = OnceLock::new();
    GROUP
        .get_or_init(|| {
            let bytes: Option<Vec<u8>> = (0..GROUP_PRIME_HEX.len())
                .step_by(2)
                .map(|at| u8::from_str_radix(GROUP_PRIME_HEX.get(at..at + 2)?, 16).ok())
                .collect();
            let bytes = bytes?;
            let modulus = MontgomeryModulus::new(&bytes)?;
            let mut order = limbs_from_be_bytes(&bytes, modulus.limb_count())?;
            let mut carried = 0u64;
            for limb in order.iter_mut().rev() {
                let shifted = (*limb >> 1) | (carried << 63);
                carried = *limb & 1;
                *limb = shifted;
            }
            Some(Group { modulus, order })
        })
        .as_ref()
}

fn exponent_of(key: &[u8]) -> Result<Vec<u64>, PsiError> {
    if !(PSI_MIN_KEY_BYTES..=PSI_MAX_KEY_BYTES).contains(&key.len()) || key.iter().all(|&byte| byte == 0) {
        return Err(PsiError::WeakKey);
    }
    limbs_from_be_bytes(key, key.len().div_ceil(8)).ok_or(PsiError::WeakKey)
}

fn hashed(element: &[u8]) -> [u8; PSI_ELEMENT_BYTES] {
    let mut out = [0u8; PSI_ELEMENT_BYTES];
    for (counter, chunk) in (0u32..).zip(out.chunks_mut(DIGEST_BYTES)) {
        let mut input = Vec::with_capacity(HASH_DOMAIN.len() + 4 + element.len());
        input.extend_from_slice(HASH_DOMAIN);
        input.extend_from_slice(&counter.to_be_bytes());
        input.extend_from_slice(element);
        chunk.copy_from_slice(&sha256(&input));
    }
    out[0] = 0;
    out
}

/// Hashes `element` into the quadratic-residue subgroup of RFC 3526's 2048-bit MODP group and
/// raises it to the secret `key`: H(element)^key. Two parties who each blind their own set and
/// then [`psi_reblind`] the other's get equal values exactly for the elements both hold, since
/// (H(x)^a)^b = (H(x)^b)^a, without either seeing the other's elements.
pub fn psi_blind(element: &[u8], key: &[u8]) -> Result<[u8; PSI_ELEMENT_BYTES], PsiError> {
    let exponent = exponent_of(key)?;
    let group = group().ok_or(PsiError::NotAnElement)?;
    let modulus = &group.modulus;
    let value = modulus.read_below(&hashed(element)).ok_or(PsiError::NotAnElement)?;
    let value = modulus.montgomery_form(&value);
    let square = modulus.multiply(&value, &value);
    finish(modulus, &modulus.power_secret(&square, &exponent))
}

/// Raises a value another party blinded to the secret `key`, after checking it is an element
/// of the prime-order subgroup other than 1, so a crafted value cannot reveal bits of the key.
pub fn psi_reblind(blinded: &[u8], key: &[u8]) -> Result<[u8; PSI_ELEMENT_BYTES], PsiError> {
    let exponent = exponent_of(key)?;
    let group = group().ok_or(PsiError::NotAnElement)?;
    let modulus = &group.modulus;
    if blinded.len() != PSI_ELEMENT_BYTES {
        return Err(PsiError::NotAnElement);
    }
    let value = modulus.read_below(blinded).ok_or(PsiError::NotAnElement)?;
    let mut one = vec![0u64; modulus.limb_count()];
    one[0] = 1;
    if value.iter().all(|&limb| limb == 0) || value == one {
        return Err(PsiError::NotAnElement);
    }
    let value = modulus.montgomery_form(&value);
    if modulus.standard_form(&modulus.power_public(&value, &group.order)) != one {
        return Err(PsiError::NotAnElement);
    }
    finish(modulus, &modulus.power_secret(&value, &exponent))
}

fn finish(modulus: &MontgomeryModulus, result: &[u64]) -> Result<[u8; PSI_ELEMENT_BYTES], PsiError> {
    let bytes = limbs_to_be_bytes(&modulus.standard_form(result), PSI_ELEMENT_BYTES);
    let mut out = [0u8; PSI_ELEMENT_BYTES];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blinding_commutes_so_shared_elements_meet() {
        let alice = [7u8; 32];
        let bob = [9u8; 32];
        let from_alice = psi_reblind(&psi_blind(b"egg@eggspot.app", &alice).unwrap(), &bob).unwrap();
        let from_bob = psi_reblind(&psi_blind(b"egg@eggspot.app", &bob).unwrap(), &alice).unwrap();
        assert_eq!(from_alice, from_bob);
        let other = psi_reblind(&psi_blind(b"hen@eggspot.app", &bob).unwrap(), &alice).unwrap();
        assert_ne!(from_alice, other);
    }

    #[test]
    fn weak_keys_and_non_elements_are_refused() {
        assert_eq!(psi_blind(b"x", &[1u8; 16]), Err(PsiError::WeakKey));
        assert_eq!(psi_blind(b"x", &[0u8; 32]), Err(PsiError::WeakKey));
        let mut one = [0u8; PSI_ELEMENT_BYTES];
        one[PSI_ELEMENT_BYTES - 1] = 1;
        assert_eq!(psi_reblind(&one, &[7u8; 32]), Err(PsiError::NotAnElement));
        let mut minus_one = [0xFFu8; PSI_ELEMENT_BYTES];
        let prime: Vec<u8> = (0..GROUP_PRIME_HEX.len()).step_by(2).map(|at| u8::from_str_radix(&GROUP_PRIME_HEX[at..at + 2], 16).unwrap()).collect();
        minus_one.copy_from_slice(&prime);
        minus_one[PSI_ELEMENT_BYTES - 1] = 0xFE;
        assert_eq!(psi_reblind(&minus_one, &[7u8; 32]), Err(PsiError::NotAnElement));
        assert_eq!(psi_blind(b"x", &[1u8; 65]), Err(PsiError::WeakKey));
    }
}
