# Test-only marker shim appended to the ASB Hermes sitecustomize hook.
if os.environ.get('ASB_FIXTURE_AGENT_BOUNDARY') == '1':
    import socket as _fixture_socket
    _fixture_invoke = transport.invoke
    _fixture_ordinal = 0
    _fixture_conn = None
    _fixture_stream = None
    def _fixture_exchange(marker):
        global _fixture_conn, _fixture_stream
        if _fixture_conn is None:
            _fixture_conn = _fixture_socket.socket(_fixture_socket.AF_UNIX, _fixture_socket.SOCK_STREAM)
            _fixture_conn.connect(os.environ['ASB_FIXTURE_EFFECT_SOCKET'])
            _fixture_stream = _fixture_conn.makefile('rwb', buffering=0)
        _fixture_stream.write((marker + '\n').encode('ascii'))
        if not _fixture_stream.readline().strip().isdigit():
            raise RuntimeError('fixture effect boundary failed')
    def _fixture_invoke_marked(*args, **kwargs):
        global _fixture_ordinal
        if _fixture_ordinal != 0:
            raise RuntimeError('fixture expected exactly one Hermes terminal call')
        _fixture_ordinal += 1
        if MODE == 'record':
            _fixture_exchange('tool-begin:one')
        value = _fixture_invoke(*args, **kwargs)
        if MODE == 'replay' and transport.failed:
            raise RuntimeError('fixture Hermes request mismatch')
        _fixture_exchange('tool-end:one')
        return value
    transport.invoke = _fixture_invoke_marked
