//! Shared cache lowering for the OpenAI protocols.
//!
//! Both OpenAI wires take the same deterministic `prompt_cache_key` and neither
//! has the explicit breakpoint vocabulary `Object` describes. Keeping the two
//! call sites on one helper is what makes the key byte-identical across the
//! two protocols.

use serde_json::{Map, Value, json};

use keel_llm_types::{
    CacheFeature, CacheObject, CacheTtl, CacheWarning, ProtocolId, Request, prompt_cache_key,
};

/// Inserts the OpenAI cache fields for `Auto` and `Object`, and reports the
/// parts of `Object` the protocol cannot express.
pub(crate) fn add_openai_cache_fields(
    protocol: ProtocolId,
    request: &Request,
    body: &mut Map<String, Value>,
    warnings: &mut Vec<CacheWarning>,
) {
    if request.cache.is_none() {
        return;
    }

    body.insert(
        "prompt_cache_key".to_owned(),
        json!(prompt_cache_key(
            protocol,
            request.model.as_str(),
            request.system.as_deref(),
            &request.tools,
        )),
    );

    if let Some(object) = request.cache.object() {
        warn_unsupported_object(protocol, object, warnings);
    }
}

fn warn_unsupported_object(
    protocol: ProtocolId,
    object: &CacheObject,
    warnings: &mut Vec<CacheWarning>,
) {
    if !object.breakpoints.is_empty() {
        warnings.push(CacheWarning::unsupported(
            protocol,
            CacheFeature::ExplicitBreakpoints {
                requested: object.breakpoints.len(),
            },
        ));
    }
    // OpenAI has no TTL field at all. The five-minute lifetime is its default,
    // so warning about it would be noise for every Object policy; a one-hour
    // request asks for a lifetime the protocol cannot express and is reported.
    if object.ttl != CacheTtl::FiveMinutes {
        warnings.push(CacheWarning::unsupported(
            protocol,
            CacheFeature::Ttl(object.ttl),
        ));
    }
}
