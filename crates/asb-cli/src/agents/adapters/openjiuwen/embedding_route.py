"""Route OpenJiuwen's embedding HTTP client without changing its identity."""

def _asb_install_embedding_route(ingress):
    if not ingress:
        return
    from urllib.parse import urlsplit

    parsed = urlsplit(ingress)
    try:
        port = parsed.port
    except ValueError as error:
        raise RuntimeError("ASB OpenJiuwen embedding tape has an invalid port") from error
    if (parsed.scheme != "http" or parsed.hostname != "127.0.0.1" or not port
            or parsed.username is not None or parsed.password is not None
            or parsed.path not in ("", "/", "/v1") or parsed.query or parsed.fragment):
        raise RuntimeError("ASB OpenJiuwen embedding tape must use an owned loopback origin")

    from openjiuwen.core.memory.lite.embeddings import OpenAICompatibleEmbeddingProvider

    original_get_client = OpenAICompatibleEmbeddingProvider._get_client

    def get_routed_client(provider):
        client = original_get_client(provider)
        if not getattr(client, "_asb_embedding_routed", False):
            async def route(request):
                # Retain path, query, body and configured provider identity.
                # The tape forwards to the originally selected upstream.
                request.url = request.url.copy_with(scheme="http", host="127.0.0.1", port=port)

            client.event_hooks.setdefault("request", []).append(route)
            client._asb_embedding_routed = True
        return client

    OpenAICompatibleEmbeddingProvider._get_client = get_routed_client


def _asb_make_embedding_identity(provider):
    """Replace this recording provider's cache identity with an opaque token.

    This prototype is intentionally opt-in and is not wired to ASB's cassette
    lifecycle. It must be installed before the provider first uses its cache.
    """
    import hashlib
    import secrets
    from openjiuwen.core.memory.lite.embeddings import OpenAICompatibleEmbeddingProvider

    if not isinstance(provider, OpenAICompatibleEmbeddingProvider):
        raise RuntimeError('ASB embedding identity needs a configured embedding provider')
    if not isinstance(provider.model, str) or not provider.model or not isinstance(provider.base_url, str) or not provider.base_url:
        raise RuntimeError('ASB embedding identity needs a model and endpoint')
    identity = {
        'model': provider.model,
        'endpoint_sha256': hashlib.sha256(provider.base_url.encode('utf-8')).hexdigest(),
        'fingerprint': 'asb-embedding-v1:' + secrets.token_hex(32),
    }
    original = OpenAICompatibleEmbeddingProvider.config_fingerprint

    def fingerprint(current):
        if current is provider:
            return identity['fingerprint']
        return original.__get__(current, type(current))

    OpenAICompatibleEmbeddingProvider.config_fingerprint = property(fingerprint)
    return identity


def _asb_restore_opaque_embedding_identity(identity):
    """Restore a fixture identity for the matching endpoint and replay key."""
    import hashlib
    import re
    from openjiuwen.core.memory.lite.embeddings import OpenAICompatibleEmbeddingProvider

    if (not isinstance(identity, dict)
            or set(identity) != {'model', 'endpoint_sha256', 'fingerprint'}
            or not isinstance(identity['model'], str) or not identity['model']
            or not isinstance(identity['endpoint_sha256'], str)
            or not re.fullmatch(r'[0-9a-f]{64}', identity['endpoint_sha256'])
            or not isinstance(identity['fingerprint'], str)
            or not re.fullmatch(r'asb-embedding-v1:[0-9a-f]{64}', identity['fingerprint'])):
        raise RuntimeError('ASB opaque embedding identity is malformed')
    identity = dict(identity)
    original = OpenAICompatibleEmbeddingProvider.config_fingerprint

    def fingerprint(provider):
        endpoint = provider.base_url
        if (isinstance(endpoint, str) and provider.model == identity['model']
                and provider.api_key == 'asb-replay-placeholder'
                and hashlib.sha256(endpoint.encode('utf-8')).hexdigest() == identity['endpoint_sha256']):
            return identity['fingerprint']
        return original.__get__(provider, type(provider))

    OpenAICompatibleEmbeddingProvider.config_fingerprint = property(fingerprint)
