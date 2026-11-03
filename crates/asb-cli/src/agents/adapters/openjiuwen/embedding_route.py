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
            or parsed.path not in ("", "/") or parsed.query or parsed.fragment):
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
