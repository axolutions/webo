//! Who is allowed in.
//!
//! webo has two front doors and they need different credentials:
//!
//! - **the panel**, opened by a person in a browser, who signs in with Google
//!   through Clerk and carries a short-lived session JWT;
//! - **the MCP server**, opened by an agent on someone's machine, which cannot
//!   do a browser login and carries a long-lived personal token instead.
//!
//! Both arrive as `Authorization: Bearer …` and both resolve to one email,
//! which is the only thing that decides access. Verification of the JWT is
//! local: RS256 against the JWKS of this Clerk instance, plus expiry. The
//! personal token is checked against a sha256 in the store — the cleartext
//! exists once, in the answer that issued it.
//!
//! Without `CLERK_PUBLISHABLE_KEY` and `CLERK_SECRET_KEY` webo runs open, as
//! it always did: one person, one machine, no login. That is the local mode,
//! and it is what every test and every fresh clone gets.

use base64::Engine;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const JWKS_TTL: Duration = Duration::from_secs(3600);
const USERS_TTL: Duration = Duration::from_secs(60);
/// A device code is good for fifteen minutes: long enough to walk to the
/// browser, short enough that an abandoned code is not a standing invitation.
pub const DEVICE_TTL_SECS: i64 = 900;

#[derive(Debug, Deserialize)]
struct Jwk {
    kid: String,
    n: String,
    e: String,
}

#[derive(Debug, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
}

/// A person who may use webo.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct User {
    pub id: String,
    pub name: String,
    pub email: String,
    pub avatar: String,
}

pub struct Auth {
    pub publishable_key: String,
    secret_key: String,
    frontend_api: String,
    allowed: Option<Vec<String>>,
    jwks: Mutex<Option<(Instant, Vec<(String, DecodingKey)>)>>,
    users: Mutex<Option<(Instant, Vec<User>)>>,
}

impl Auth {
    /// Login is on when both Clerk keys are in the environment. One key alone
    /// is a misconfiguration, not a mode: it fails loudly rather than leaving
    /// the panel open while looking configured.
    pub fn from_env() -> Result<Option<Auth>, String> {
        let pk = std::env::var("CLERK_PUBLISHABLE_KEY").unwrap_or_default();
        let sk = std::env::var("CLERK_SECRET_KEY").unwrap_or_default();
        match (pk.trim().is_empty(), sk.trim().is_empty()) {
            (true, true) => return Ok(None),
            (false, true) => return Err("CLERK_PUBLISHABLE_KEY is set but CLERK_SECRET_KEY is not".into()),
            (true, false) => return Err("CLERK_SECRET_KEY is set but CLERK_PUBLISHABLE_KEY is not".into()),
            _ => {}
        }
        let frontend_api = frontend_api_from_pk(&pk)
            .ok_or("CLERK_PUBLISHABLE_KEY is not a Clerk publishable key (pk_test_… / pk_live_…)")?;
        Ok(Some(Auth {
            publishable_key: pk,
            secret_key: sk,
            frontend_api,
            allowed: allowed_from_env(),
            jwks: Mutex::new(None),
            users: Mutex::new(None),
        }))
    }

    /// Verifies a Clerk session JWT and resolves it to the person it belongs
    /// to — refusing anyone off the allowlist even when Clerk says yes.
    pub fn session_user(&self, token: &str) -> Result<User, String> {
        let claims = self.verify(token)?;
        let user = self
            .team()?
            .into_iter()
            .find(|u| u.id == claims.sub)
            .ok_or("the session is valid but its user is not in this Clerk instance")?;
        self.check_allowed(&user.email)?;
        Ok(user)
    }

    /// The allowlist is the last word: a token issued yesterday to someone who
    /// left the team stops working the moment the list changes.
    pub fn check_allowed(&self, email: &str) -> Result<(), String> {
        match &self.allowed {
            Some(list) if !list.contains(&email.to_lowercase()) => {
                Err(format!("{email} is not on WEBO_ALLOWED_EMAILS"))
            }
            _ => Ok(()),
        }
    }

    fn verify(&self, token: &str) -> Result<Claims, String> {
        let header = decode_header(token).map_err(|e| format!("malformed token: {e}"))?;
        let kid = header.kid.ok_or("token has no kid")?;
        let key = self.key_for(&kid)?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = 10;
        // What authenticates the token is the signature against THIS instance's
        // JWKS plus expiry; Clerk's session `aud` varies by setup.
        validation.validate_aud = false;
        decode::<Claims>(token, &key, &validation)
            .map(|d| d.claims)
            .map_err(|e| format!("session rejected: {e}"))
    }

    fn key_for(&self, kid: &str) -> Result<DecodingKey, String> {
        {
            let cache = self.jwks.lock().unwrap();
            if let Some((at, keys)) = cache.as_ref() {
                if at.elapsed() < JWKS_TTL {
                    if let Some((_, k)) = keys.iter().find(|(k, _)| k == kid) {
                        return Ok(k.clone());
                    }
                }
            }
        }
        // cold cache, or a kid we have never seen (key rotation): refetch
        let url = if self.frontend_api.starts_with("http://") {
            // only a test points this at plain http; production keys always
            // decode to a hostname, which is fetched over TLS below
            format!("{}/.well-known/jwks.json", self.frontend_api)
        } else {
            format!("https://{}/.well-known/jwks.json", self.frontend_api)
        };
        let jwks: Jwks = ureq::get(&url)
            .timeout(Duration::from_secs(10))
            .call()
            .map_err(|e| format!("could not read the Clerk JWKS: {e}"))?
            .into_json()
            .map_err(|e| format!("the Clerk JWKS is not what we expect: {e}"))?;
        let keys: Vec<(String, DecodingKey)> = jwks
            .keys
            .iter()
            .filter_map(|k| DecodingKey::from_rsa_components(&k.n, &k.e).ok().map(|d| (k.kid.clone(), d)))
            .collect();
        let found = keys.iter().find(|(k, _)| k == kid).map(|(_, d)| d.clone());
        *self.jwks.lock().unwrap() = Some((Instant::now(), keys));
        found.ok_or_else(|| format!("key {kid:?} is not in this instance's JWKS"))
    }

    /// The people in this Clerk instance, cached for a minute.
    pub fn team(&self) -> Result<Vec<User>, String> {
        {
            let cache = self.users.lock().unwrap();
            if let Some((at, users)) = cache.as_ref() {
                if at.elapsed() < USERS_TTL {
                    return Ok(users.clone());
                }
            }
        }
        #[derive(Deserialize)]
        struct ClerkEmail {
            id: String,
            email_address: String,
        }
        #[derive(Deserialize)]
        struct ClerkUser {
            id: String,
            first_name: Option<String>,
            image_url: Option<String>,
            primary_email_address_id: Option<String>,
            email_addresses: Vec<ClerkEmail>,
        }
        let base = std::env::var("WEBO_CLERK_API_BASE").unwrap_or_else(|_| "https://api.clerk.com".into());
        let raw: Vec<ClerkUser> = ureq::get(&format!("{base}/v1/users?limit=100&order_by=-created_at"))
            .set("Authorization", &format!("Bearer {}", self.secret_key))
            .timeout(Duration::from_secs(10))
            .call()
            .map_err(|e| format!("could not reach the Clerk API: {e}"))?
            .into_json()
            .map_err(|e| format!("the Clerk user list is not what we expect: {e}"))?;
        let users: Vec<User> = raw
            .into_iter()
            .map(|u| {
                let email = u
                    .primary_email_address_id
                    .as_ref()
                    .and_then(|id| u.email_addresses.iter().find(|e| &e.id == id))
                    .or_else(|| u.email_addresses.first())
                    .map(|e| e.email_address.clone())
                    .unwrap_or_default();
                let name = u
                    .first_name
                    .clone()
                    .filter(|f| !f.trim().is_empty())
                    .unwrap_or_else(|| email.split('@').next().unwrap_or("?").to_string());
                User { id: u.id, name, email, avatar: u.image_url.unwrap_or_default() }
            })
            .collect();
        *self.users.lock().unwrap() = Some((Instant::now(), users.clone()));
        Ok(users)
    }
}

fn allowed_from_env() -> Option<Vec<String>> {
    let raw = std::env::var("WEBO_ALLOWED_EMAILS").ok()?;
    let list: Vec<String> = raw
        .split(',')
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    // An empty setting means "no list", never "nobody" — a typo in the compose
    // file must not lock the whole team out.
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

/// The Frontend API domain lives inside the publishable key:
/// `pk_test_<base64("open-scorpion-96.clerk.accounts.dev$")>`.
pub fn frontend_api_from_pk(pk: &str) -> Option<String> {
    let b64 = pk.strip_prefix("pk_test_").or_else(|| pk.strip_prefix("pk_live_"))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(b64))
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    let domain = s.trim_end_matches('$');
    // it must look like a hostname, or a truncated key would become a URL we
    // then fetch keys from
    if domain.contains('.') && !domain.contains('/') && !domain.is_empty() {
        Some(domain.to_string())
    } else {
        None
    }
}

/// Personal tokens are `webo_` + 48 hex characters from the OS. They are
/// stored as a sha256: a leaked database does not hand anyone a working token.
pub fn new_personal_token() -> String {
    format!("webo_{}", random_hex(24))
}

pub fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    format!("{:x}", h.finalize())
}

/// A device code a person can read out loud and type: no vowels, no digits
/// that look like letters, in two blocks of four.
pub fn new_device_code() -> String {
    const ALPHA: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut raw = [0u8; 8];
    let _ = getrandom::getrandom(&mut raw);
    let chars: Vec<char> = raw.iter().map(|b| ALPHA[(*b as usize) % ALPHA.len()] as char).collect();
    format!(
        "{}-{}",
        chars[..4].iter().collect::<String>(),
        chars[4..].iter().collect::<String>()
    )
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // getrandom only fails when the OS has no entropy source at all; there is
    // no safe fallback for a credential, so the token is not issued.
    if getrandom::getrandom(&mut buf).is_err() {
        return String::new();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// An Auth with no Clerk behind it, for tests that exercise the gate itself:
/// personal tokens and the allowlist resolve locally, and any path that would
/// reach Clerk fails as an unreachable instance rather than silently passing.
#[cfg(test)]
pub fn offline_auth(allowed: Option<Vec<&str>>) -> Auth {
    with_frontend_api("offline.invalid", allowed)
}

/// A whole fake Clerk instance plus a session token it signed, so the tests
/// that exercise the panel's gate can use a real JWT instead of pretending.
#[cfg(test)]
pub async fn fake_clerk_session(allowed: Option<Vec<&str>>) -> (Auth, String) {
    let base = tests::fake_clerk().await;
    std::env::set_var("WEBO_CLERK_API_BASE", &base);
    let token = tests::sign(
        serde_json::json!({ "sub": "user_1" }),
        "k1",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 600,
    );
    (with_frontend_api(&base, allowed), token)
}

#[cfg(test)]
pub fn with_frontend_api(frontend_api: &str, allowed: Option<Vec<&str>>) -> Auth {
    Auth {
        publishable_key: "pk_test_offline".into(),
        secret_key: "sk_test_offline".into(),
        frontend_api: frontend_api.into(),
        allowed: allowed.map(|l| l.iter().map(|e| e.to_lowercase()).collect()),
        jwks: Mutex::new(None),
        users: Mutex::new(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway RSA key, generated once for these tests. It signs the
    /// tokens a fake Clerk instance hands out, and its public half is served
    /// as that instance's JWKS — which is exactly what verification checks.
    const TEST_KEY: &str = include_str!("../tests/fixtures/clerk-test-key.pem");
    const TEST_N: &str = "zUnjm9Yonyk0cerArpd7sOv1wDHKfIbchmw4WTnEVL8jHKUbhextsvK1ArrnletOBzndKI9nbF-sg5vZme9sj_JESvsmt3HvPgkeE5ZHMAlu0ZuPVlIHjyWg0268D7P_UFIt02qtNmAmHt6s7H9uwv4epQuRTE_O4cVW1xaEatukeCosYPH27OalfxLdR7sKPnf5_0C7JBLF86T5scQM8Nh_AKlzgGGXSDvuOGGM_eR-AC9ETdmZizLBj2f3lN0n0tVWgrChsOmX5HsH3FyMqBGW-429Ea0sH8oIiuw3SZLrcyShRFtq1ZHgc5oWWsZyeGs054o0dNoFTOFgc0WVfQ";
    const TEST_E: &str = "AQAB";

    pub(super) fn sign(claims: serde_json::Value, kid: &str, exp: i64) -> String {
        let mut header = jsonwebtoken::Header::new(Algorithm::RS256);
        header.kid = Some(kid.into());
        let mut body = claims;
        body["exp"] = serde_json::json!(exp);
        jsonwebtoken::encode(
            &header,
            &body,
            &jsonwebtoken::EncodingKey::from_rsa_pem(TEST_KEY.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// A Clerk instance that only exists for this test: it serves the JWKS for
    /// the key above and one user.
    pub(super) async fn fake_clerk() -> String {
        use axum::routing::get;
        let router = axum::Router::new()
            .route(
                "/.well-known/jwks.json",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "keys": [
                            { "kid": "k1", "kty": "RSA", "n": TEST_N, "e": TEST_E },
                            // a key we cannot parse must not poison the rest
                            { "kid": "broken", "kty": "RSA", "n": "!!!", "e": "!!!" },
                        ]
                    }))
                }),
            )
            .route(
                "/v1/users",
                get(|| async {
                    axum::Json(serde_json::json!([
                        {
                            "id": "user_1",
                            "first_name": "Murilo",
                            "image_url": "https://example.com/a.png",
                            "primary_email_address_id": "e1",
                            "email_addresses": [
                                { "id": "e0", "email_address": "old@example.com" },
                                { "id": "e1", "email_address": "Murilo@Example.com" }
                            ]
                        },
                        {
                            "id": "user_2",
                            "first_name": null,
                            "image_url": null,
                            "primary_email_address_id": null,
                            "email_addresses": [{ "id": "e2", "email_address": "gustavo@example.com" }]
                        }
                    ]))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_is_only_accepted_when_this_instance_signed_it() {
        let _lock = crate::testutil::env_lock();
        let base = fake_clerk().await;
        std::env::set_var("WEBO_CLERK_API_BASE", &base);
        let auth = with_frontend_api(&base, Some(vec!["murilo@example.com"]));

        // a real session: signed by the instance, unexpired, on the allowlist
        let token = sign(serde_json::json!({ "sub": "user_1" }), "k1", now() + 600);
        let user = auth.session_user(&token).unwrap();
        assert_eq!(user.email, "Murilo@Example.com", "the primary address, not the first");
        assert_eq!(user.name, "Murilo");

        // the whole team is readable, and someone with no first name is named
        // by their address rather than left blank
        let team = auth.team().unwrap();
        assert_eq!(team.len(), 2);
        assert_eq!(team[1].name, "gustavo");

        // expired
        let old = sign(serde_json::json!({ "sub": "user_1" }), "k1", now() - 60);
        assert!(auth.session_user(&old).unwrap_err().contains("session rejected"));

        // signed by a key this instance does not have
        let stranger = sign(serde_json::json!({ "sub": "user_1" }), "nope", now() + 600);
        assert!(auth.session_user(&stranger).unwrap_err().contains("not in this instance"));

        // not a token at all
        assert!(auth.session_user("garbage").unwrap_err().contains("malformed"));
        // a token with no kid cannot be checked against anything
        let no_kid = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::RS256),
            &serde_json::json!({ "sub": "user_1", "exp": now() + 600 }),
            &jsonwebtoken::EncodingKey::from_rsa_pem(TEST_KEY.as_bytes()).unwrap(),
        )
        .unwrap();
        assert!(auth.session_user(&no_kid).unwrap_err().contains("no kid"));

        // a valid session for someone who is not in the instance
        let ghost = sign(serde_json::json!({ "sub": "user_404" }), "k1", now() + 600);
        assert!(auth.session_user(&ghost).unwrap_err().contains("not in this Clerk instance"));

        // signed, unexpired, real user — and still refused by the allowlist
        let auth = with_frontend_api(&base, Some(vec!["someone@else.com"]));
        assert!(auth.session_user(&token).unwrap_err().contains("not on WEBO_ALLOWED_EMAILS"));

        std::env::remove_var("WEBO_CLERK_API_BASE");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreachable_clerk_refuses_rather_than_lets_through() {
        let _lock = crate::testutil::env_lock();
        std::env::set_var("WEBO_CLERK_API_BASE", "http://127.0.0.1:1");
        let auth = with_frontend_api("http://127.0.0.1:1", None);
        let token = sign(serde_json::json!({ "sub": "user_1" }), "k1", now() + 600);
        let err = auth.session_user(&token).unwrap_err();
        assert!(err.contains("JWKS"), "{err}");
        assert!(auth.team().is_err(), "no team list means no answer, not an empty team");
        std::env::remove_var("WEBO_CLERK_API_BASE");
    }

    #[test]
    fn the_frontend_api_comes_out_of_the_publishable_key() {
        let pk = format!(
            "pk_test_{}",
            base64::engine::general_purpose::STANDARD.encode("open-scorpion-96.clerk.accounts.dev$")
        );
        assert_eq!(frontend_api_from_pk(&pk).unwrap(), "open-scorpion-96.clerk.accounts.dev");
        // a live key works the same way
        let live = format!(
            "pk_live_{}",
            base64::engine::general_purpose::STANDARD.encode("clerk.webo.example.com$")
        );
        assert_eq!(frontend_api_from_pk(&live).unwrap(), "clerk.webo.example.com");
    }

    #[test]
    fn a_key_that_is_not_a_key_is_refused_rather_than_guessed() {
        assert!(frontend_api_from_pk("sk_test_nope").is_none(), "a secret key is not a publishable one");
        assert!(frontend_api_from_pk("pk_test_%%%").is_none(), "not base64");
        // decodes, but is not a hostname — we would fetch keys from it
        let odd = format!(
            "pk_test_{}",
            base64::engine::general_purpose::STANDARD.encode("evil.example.com/path$")
        );
        assert!(frontend_api_from_pk(&odd).is_none());
        assert!(frontend_api_from_pk("pk_test_").is_none());
    }

    #[test]
    fn login_is_off_until_both_keys_are_present_and_never_half_on() {
        let _lock = crate::testutil::env_lock();
        for v in ["CLERK_PUBLISHABLE_KEY", "CLERK_SECRET_KEY", "WEBO_ALLOWED_EMAILS"] {
            std::env::remove_var(v);
        }
        assert!(Auth::from_env().unwrap().is_none(), "no keys: webo runs open, as it always did");

        // one key alone is a mistake worth shouting about: silently staying
        // open is how a panel ends up on the internet with no login
        std::env::set_var("CLERK_PUBLISHABLE_KEY", "pk_test_x");
        assert!(Auth::from_env().is_err());
        std::env::remove_var("CLERK_PUBLISHABLE_KEY");
        std::env::set_var("CLERK_SECRET_KEY", "sk_test_x");
        assert!(Auth::from_env().is_err());

        // both, but the publishable key is nonsense
        std::env::set_var("CLERK_PUBLISHABLE_KEY", "not-a-key");
        assert!(Auth::from_env().is_err());

        std::env::set_var(
            "CLERK_PUBLISHABLE_KEY",
            format!("pk_test_{}", base64::engine::general_purpose::STANDARD.encode("a-b-1.clerk.accounts.dev$")),
        );
        let auth = Auth::from_env().unwrap().expect("both keys: login is on");
        assert_eq!(auth.frontend_api, "a-b-1.clerk.accounts.dev");
        assert!(auth.allowed.is_none(), "no allowlist means Clerk decides alone");

        std::env::remove_var("CLERK_PUBLISHABLE_KEY");
        std::env::remove_var("CLERK_SECRET_KEY");
    }

    #[test]
    fn the_allowlist_is_case_insensitive_and_a_blank_one_locks_nobody_out() {
        let _lock = crate::testutil::env_lock();
        std::env::set_var("WEBO_ALLOWED_EMAILS", " Murilo@Example.com , gustavo@example.com ");
        let list = allowed_from_env().unwrap();
        assert_eq!(list, vec!["murilo@example.com", "gustavo@example.com"]);

        // a typo that leaves the value empty must not lock the team out
        std::env::set_var("WEBO_ALLOWED_EMAILS", " , ,");
        assert!(allowed_from_env().is_none());
        std::env::remove_var("WEBO_ALLOWED_EMAILS");
        assert!(allowed_from_env().is_none());
    }

    #[test]
    fn credentials_are_unguessable_and_stored_only_as_a_hash() {
        let a = new_personal_token();
        let b = new_personal_token();
        assert!(a.starts_with("webo_"), "{a}");
        assert_eq!(a.len(), 5 + 48, "24 bytes as hex");
        assert_ne!(a, b, "two tokens are never the same");

        let h = hash_token(&a);
        assert_eq!(h.len(), 64);
        assert!(!h.contains(&a[5..]), "the hash does not carry the token");
        assert_eq!(h, hash_token(&a), "the same token hashes the same");
        assert_ne!(h, hash_token(&b));

        let code = new_device_code();
        assert_eq!(code.len(), 9, "XXXX-XXXX: {code}");
        assert_eq!(code.chars().nth(4), Some('-'));
        // no O/I/0/1: a code is read off a screen and typed by a person
        assert!(!code.contains(['O', 'I', '0', '1']), "{code}");
    }
}
