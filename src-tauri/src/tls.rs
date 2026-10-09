//! Сертификаты для HTTPS: встроенный список (webpki-roots) плюс хранилище Windows.
//!
//! Только встроенного мало: антивирусы с проверкой защищенных соединений (Касперский, ESET и др.)
//! подменяют сертификат сайта своим, а свой корневой кладут в хранилище Windows — браузер ему
//! верит, а приложение со встроенным списком не подключалось бы. Только хранилища Windows тоже мало:
//! на свежей системе часть корневых Windows подгружает по требованию, нужного может еще не быть.

use std::sync::{Arc, OnceLock};

fn config() -> Arc<rustls::ClientConfig> {
    static CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let mut roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let native = rustls_native_certs::load_native_certs();
        let (added, _) = roots.add_parsable_certificates(native.certs);
        crate::log(&format!("сертификаты: встроенные и {added} из хранилища Windows"));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("протоколы TLS")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(cfg)
    })
    .clone()
}

/// ureq::AgentBuilder с нашими сертификатами — вместо AgentBuilder::new() везде.
pub fn agent() -> ureq::AgentBuilder {
    ureq::AgentBuilder::new().tls_config(config())
}
