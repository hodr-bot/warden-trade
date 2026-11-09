use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

/// JWT claims: `sub` carries the user_id, plus multi-tenant `tenant_id`, plus `exp`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub tenant_id: String,
    /// Password version at issue time. `default` keeps pre-0007 tokens
    /// decodable: they carry pv 0, which still matches any never-rotated row.
    #[serde(default)]
    pub pv: i32,
    pub exp: usize,
}

/// Hash a password with argon2 using the default config.
///
/// Returns the PHC-encoded hash string on success.
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string();
    Ok(hash)
}

/// Verify a password against an argon2 PHC hash.
///
/// Returns `true` on match, `false` on mismatch or malformed hash.
/// Never panics and never leaks whether the hash was malformed.
pub fn verify_password(hash: &str, password: &str) -> bool {
    let parsed = match PasswordHash::new(hash) {
        Ok(h) => h,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Encode a JWT with HS256. `sub` = user_id, plus tenant_id, plus exp (unix timestamp).
pub fn encode_jwt(
    user_id: &str,
    tenant_id: &str,
    pwd_ver: i32,
    exp: usize,
    secret: &[u8],
) -> Result<String, jsonwebtoken::errors::Error> {
    let claims = Claims {
        sub: user_id.to_owned(),
        tenant_id: tenant_id.to_owned(),
        pv: pwd_ver,
        exp,
    };
    let header = Header::new(Algorithm::HS256);
    encode_jwt_claims(&claims, secret, header)
}

fn encode_jwt_claims(
    claims: &Claims,
    secret: &[u8],
    header: Header,
) -> Result<String, jsonwebtoken::errors::Error> {
    debug_assert_eq!(header.alg, Algorithm::HS256);
    jsonwebtoken::encode(&header, claims, &EncodingKey::from_secret(secret))
}

/// Decode and validate a JWT with HS256 (signature + exp enforced).
pub fn decode_jwt(
    token: &str,
    secret: &[u8],
) -> Result<Claims, jsonwebtoken::errors::Error> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    let data = jsonwebtoken::decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret),
        &validation,
    )?;
    Ok(data.claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn future_exp() -> usize {
        (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize
    }

    #[test]
    fn hash_verify_roundtrip() {
        let password = "correct-horse-battery-staple-42";
        let hash = hash_password(password).expect("hashing must succeed");
        assert_ne!(hash, password, "hash must not equal plaintext");
        assert!(
            hash.starts_with("$argon2"),
            "hash must be PHC-encoded, got: {hash}"
        );
        assert!(
            verify_password(&hash, password),
            "correct password must verify"
        );
    }

    #[test]
    fn wrong_password_fails() {
        let hash = hash_password("the-right-password").expect("hashing must succeed");
        assert!(
            !verify_password(&hash, "the-wrong-password"),
            "wrong password must not verify"
        );
        assert!(
            !verify_password("not-a-valid-hash", "anything"),
            "malformed hash must return false"
        );
    }

    #[test]
    fn jwt_roundtrip() {
        let secret = b"test-secret-must-be-long-enough-0123456789";
        let user_id = "11111111-1111-4111-8111-111111111111";
        let tenant_id = "22222222-2222-4222-8222-222222222222";
        let exp = future_exp();

        let token = encode_jwt(user_id, tenant_id, 0, exp, secret).expect("encode must succeed");
        assert_eq!(
            token.matches('.').count(),
            2,
            "JWT must have header.payload.signature shape"
        );

        let claims = decode_jwt(&token, secret).expect("decode must succeed");
        assert_eq!(claims.sub, user_id);
        assert_eq!(claims.tenant_id, tenant_id);
        assert_eq!(claims.exp, exp);

        // Wrong secret must fail.
        assert!(
            decode_jwt(&token, b"a-different-secret-000000000000000000").is_err(),
            "token verified with wrong secret must fail"
        );
    }
}
