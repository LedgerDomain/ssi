use async_trait::async_trait;

use ssi_dids::did_resolve::{
    DIDResolver, DocumentMetadata, ResolutionInputMetadata, ResolutionMetadata, ERROR_INVALID_DID,
    ERROR_NOT_FOUND, TYPE_DID_LD_JSON,
};
use ssi_dids::{DIDMethod, Document};
pub const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

// For testing, enable handling requests at localhost.
#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
thread_local! {
  static PROXY: RefCell<Option<String>> = RefCell::new(None);
}

/// did:web Method
///
/// [Specification](https://w3c-ccg.github.io/did-method-web/)
///
/// DIDWeb struct has an HTTP client to use for DID resolution.  It's incredibly slow to create a new
/// reqwest::Client, due to the overhead of loading the system's root certificates.  This HTTP client must
/// be specified by constructing the DIDWeb instance using new_with_default_http_client (to use defaults)
/// or DIDWeb::new_with_http_client if there is an specific reqwest::Client that should be reused.
/// Note that this is the recommended approach to using reqwest::Client (see
/// https://docs.rs/reqwest/latest/reqwest/struct.Client.html).
pub struct DIDWeb {
    http_client: reqwest::Client,
}

impl DIDWeb {
    /// Create an instance of the DIDWeb resolver with a default HTTP client.  See also `DIDWeb::new_with_http_client`.
    pub fn new_with_default_http_client() -> Result<Self, String> {
        let mut headers = reqwest::header::HeaderMap::new();

        headers.insert(
            "User-Agent",
            reqwest::header::HeaderValue::from_static(USER_AGENT),
        );

        let http_client = match reqwest::Client::builder().default_headers(headers).build() {
            Ok(http_client) => http_client,
            Err(err) => {
                return Err(format!("Error building HTTP client: {}", err));
            }
        };

        Ok(Self { http_client })
    }
    /// Create an instance of the DIDWeb resolver with a specific HTTP client.  See also
    /// `DIDWeb::new_with_default_http_client`.
    pub fn new_with_http_client(http_client: reqwest::Client) -> Self {
        Self { http_client }
    }
}

fn did_web_url(did: &str) -> Result<String, ResolutionMetadata> {
    let mut parts = did.split(':').peekable();
    let domain_name = match (parts.next(), parts.next(), parts.next()) {
        (Some("did"), Some("web"), Some(domain_name)) if !domain_name.is_empty() => domain_name,
        _ => {
            return Err(ResolutionMetadata::from_error(ERROR_INVALID_DID));
        }
    };
    // TODO:
    // - Validate domain name: alphanumeric, hyphen, dot. no IP address.
    // - Ensure domain name matches TLS certificate common name
    // - Support punycode?
    // - Support query strings?
    let path = match parts.peek() {
        Some(_) => parts.collect::<Vec<&str>>().join("/"),
        None => ".well-known".to_string(),
    };

    // If the env var is set (it should be a comma-delimited sequence of hostnames for which the did:web resolution
    // process should resolve to a "http://" URL instead of "https://" URL), then use it.  Otherwise, default to
    // "localhost".
    let force_http_for_hostnames_string = std::env::var("SSI__DID_WEB__FORCE_HTTP_FOR_HOSTNAMES")
        .unwrap_or_else(|_| "localhost".to_string());
    let force_http_for_hostnames = force_http_for_hostnames_string.split(',');

    // Determine if http should be used, or https.
    let proto = if force_http_for_hostnames
        .into_iter()
        .any(|force_http_for_hostname| {
            domain_name
                .split("%3A")
                .next()
                .expect("programmer error: domain_name should have been nonempty")
                == force_http_for_hostname
        }) {
        "http"
    } else {
        "https"
    };

    #[allow(unused_mut)]
    let mut url = format!(
        "{}://{}/{}/did.json",
        proto,
        domain_name.replacen("%3A", ":", 1),
        path
    );
    #[cfg(test)]
    PROXY.with(|proxy| {
        if let Some(ref proxy) = *proxy.borrow() {
            url = proxy.clone() + &url;
        }
    });
    Ok(url)
}

/// <https://w3c-ccg.github.io/did-method-web/#read-resolve>
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl DIDResolver for DIDWeb {
    async fn resolve(
        &self,
        did: &str,
        input_metadata: &ResolutionInputMetadata,
    ) -> (
        ResolutionMetadata,
        Option<Document>,
        Option<DocumentMetadata>,
    ) {
        let (mut res_meta, doc_data, doc_meta_opt) =
            self.resolve_representation(did, input_metadata).await;
        let doc_opt = if doc_data.is_empty() {
            None
        } else {
            match serde_json::from_slice(&doc_data) {
                Ok(doc) => doc,
                Err(err) => {
                    return (
                        ResolutionMetadata::from_error(
                            &("JSON Error: ".to_string() + &err.to_string()),
                        ),
                        None,
                        None,
                    )
                }
            }
        };
        // https://www.w3.org/TR/did-core/#did-resolution-metadata
        // contentType - "MUST NOT be present if the resolve function was called"
        res_meta.content_type = None;
        (res_meta, doc_opt, doc_meta_opt)
    }

    async fn resolve_representation(
        &self,
        did: &str,
        input_metadata: &ResolutionInputMetadata,
    ) -> (ResolutionMetadata, Vec<u8>, Option<DocumentMetadata>) {
        let url = match did_web_url(did) {
            Err(meta) => return (meta, Vec::new(), None),
            Ok(url) => url,
        };
        // TODO: https://w3c-ccg.github.io/did-method-web/#in-transit-security

        let accept = input_metadata
            .accept
            .clone()
            .unwrap_or_else(|| "application/json".to_string());
        let resp = match self
            .http_client
            .get(&url)
            .header("Accept", accept)
            .send()
            .await
        {
            Ok(req) => req,
            Err(err) => {
                return (
                    ResolutionMetadata::from_error(&format!(
                        "Error sending HTTP request ({}): {}",
                        url, err
                    )),
                    Vec::new(),
                    None,
                )
            }
        };
        if let Err(err) = resp.error_for_status_ref() {
            if err.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                return (
                    ResolutionMetadata::from_error(ERROR_NOT_FOUND),
                    Vec::new(),
                    Some(DocumentMetadata::default()),
                );
            }
            return (
                ResolutionMetadata::from_error(&err.to_string()),
                Vec::new(),
                Some(DocumentMetadata::default()),
            );
        }
        let doc_representation = match resp.bytes().await {
            Ok(bytes) => bytes.to_vec(),
            Err(err) => {
                return (
                    ResolutionMetadata::from_error(
                        &("Error reading HTTP response: ".to_string() + &err.to_string()),
                    ),
                    Vec::new(),
                    None,
                )
            }
        };
        // TODO: set document created/updated metadata from HTTP headers?
        (
            ResolutionMetadata {
                error: None,
                content_type: Some(TYPE_DID_LD_JSON.to_string()),
                property_set: None,
            },
            doc_representation,
            Some(DocumentMetadata::default()),
        )
    }
}

impl DIDMethod for DIDWeb {
    fn name(&self) -> &'static str {
        "web"
    }

    fn to_resolver(&self) -> &dyn DIDResolver {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[async_std::test]
    async fn parse_did_web() {
        // https://w3c-ccg.github.io/did-method-web/#example-3-creating-the-did
        assert_eq!(
            did_web_url("did:web:w3c-ccg.github.io").unwrap(),
            "https://w3c-ccg.github.io/.well-known/did.json"
        );
        // https://w3c-ccg.github.io/did-method-web/#example-4-creating-the-did-with-optional-path
        assert_eq!(
            did_web_url("did:web:w3c-ccg.github.io:user:alice").unwrap(),
            "https://w3c-ccg.github.io/user/alice/did.json"
        );
        // https://w3c-ccg.github.io/did-method-web/#optional-path-considerations
        assert_eq!(
            did_web_url("did:web:example.com:u:bob").unwrap(),
            "https://example.com/u/bob/did.json"
        );
        // https://w3c-ccg.github.io/did-method-web/#example-creating-the-did-with-optional-path-and-port
        assert_eq!(
            did_web_url("did:web:example.com%3A443:u:bob").unwrap(),
            "https://example.com:443/u/bob/did.json"
        );
    }

    const DID_URL: &str = "http://localhost/.well-known/did.json";
    const DID_JSON: &str = r#"{
      "@context": "https://www.w3.org/ns/did/v1",
      "id": "did:web:localhost",
      "verificationMethod": [{
         "id": "did:web:localhost#key1",
         "type": "Ed25519VerificationKey2018",
         "controller": "did:web:localhost",
         "publicKeyJwk": {
           "key_id": "ed25519-2020-10-18",
           "kty": "OKP",
           "crv": "Ed25519",
           "x": "G80iskrv_nE69qbGLSpeOHJgmV4MKIzsy5l5iT6pCww"
         }
      }],
      "assertionMethod": ["did:web:localhost#key1"]
    }"#;

    // localhost web server for serving did:web DID documents.
    // TODO: pass arguments here instead of using const
    fn web_server() -> Result<(String, impl FnOnce() -> Result<(), ()>), hyper::Error> {
        use http::header::{HeaderValue, CONTENT_TYPE};
        use hyper::service::{make_service_fn, service_fn};
        use hyper::{Body, Response, Server};
        let addr = ([127, 0, 0, 1], 0).into();
        let make_svc = make_service_fn(|_| async move {
            Ok::<_, hyper::Error>(service_fn(|req| async move {
                let uri = req.uri();
                // Skip leading slash
                let proxied_url: String = uri.path().chars().skip(1).collect();
                if proxied_url == DID_URL {
                    let body = Body::from(DID_JSON);
                    let mut response = Response::new(body);
                    response
                        .headers_mut()
                        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                    return Ok::<_, hyper::Error>(response);
                }

                let (mut parts, body) = Response::<Body>::default().into_parts();
                parts.status = hyper::StatusCode::NOT_FOUND;
                let response = Response::from_parts(parts, body);
                Ok::<_, hyper::Error>(response)
            }))
        });
        let server = Server::try_bind(&addr)?.serve(make_svc);
        let url = "http://".to_string() + &server.local_addr().to_string() + "/";
        let (shutdown_tx, shutdown_rx) = futures::channel::oneshot::channel();
        let graceful = server.with_graceful_shutdown(async {
            shutdown_rx.await.ok();
        });
        tokio::task::spawn(async move {
            graceful.await.ok();
        });
        let shutdown = || shutdown_tx.send(());
        Ok((url, shutdown))
    }

    #[tokio::test]
    async fn from_did_key() {
        let (url, shutdown) = web_server().unwrap();
        PROXY.with(|proxy| {
            proxy.replace(Some(url));
        });
        let did_web_resolver = DIDWeb::new_with_default_http_client().unwrap();
        let (res_meta, doc_opt, _doc_meta) = did_web_resolver
            .resolve("did:web:localhost", &ResolutionInputMetadata::default())
            .await;
        assert_eq!(res_meta.error, None);
        let doc_expected: Document = serde_json::from_str(DID_JSON).unwrap();
        assert_eq!(doc_opt, Some(doc_expected));
        PROXY.with(|proxy| {
            proxy.replace(None);
        });
        shutdown().ok();
    }

    #[tokio::test]
    async fn credential_prove_verify_did_web() {
        use ssi_jwk::JWK;
        use ssi_ldp::LinkedDataProofOptions;
        use ssi_vc::{Credential, Issuer, URI};
        let vc_str = r###"{
            "@context": "https://www.w3.org/2018/credentials/v1",
            "type": ["VerifiableCredential"],
            "issuer": "did:web:localhost",
            "issuanceDate": "2021-01-26T16:57:27Z",
            "credentialSubject": {
                "id": "did:web:localhost"
            }
        }"###;
        let (url, shutdown) = web_server().unwrap();
        PROXY.with(|proxy| {
            proxy.replace(Some(url));
        });
        let mut vc: Credential = Credential::from_json_unsigned(vc_str).unwrap();
        let key_str = include_str!("../../tests/ed25519-2020-10-18.json");
        let key: JWK = serde_json::from_str(key_str).unwrap();
        let issue_options = LinkedDataProofOptions {
            verification_method: Some(URI::String("did:web:localhost#key1".to_string())),
            ..Default::default()
        };
        let mut context_loader = ssi_json_ld::ContextLoader::default();
        let did_web_resolver = DIDWeb::new_with_default_http_client().unwrap();
        let proof = vc
            .generate_proof(&key, &issue_options, &did_web_resolver, &mut context_loader)
            .await
            .unwrap();
        println!("{}", serde_json::to_string_pretty(&proof).unwrap());
        vc.add_proof(proof);
        vc.validate().unwrap();
        let verification_result = vc
            .verify(None, &did_web_resolver, &mut context_loader)
            .await;
        println!("{:#?}", verification_result);
        assert!(verification_result.errors.is_empty());

        // test that issuer property is used for verification
        vc.issuer = Some(Issuer::URI(URI::String("did:example:bad".to_string())));
        assert!(!vc
            .verify(None, &did_web_resolver, &mut context_loader)
            .await
            .errors
            .is_empty());

        PROXY.with(|proxy| {
            proxy.replace(None);
        });
        shutdown().ok();
    }
}
