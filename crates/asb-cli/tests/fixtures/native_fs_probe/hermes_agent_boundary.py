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

if os.environ.get('ASB_FIXTURE_NATIVE_FILES') == '1':
    # Private test transcript: production Hermes does not yet implement this.
    import tools.file_tools as _fixture_file_tools
    from tools.registry import registry as _fixture_registry
    _fixture_file_path = Path(os.environ['ASB_FIXTURE_NATIVE_FILE_TRANSCRIPT'])
    _fixture_file_events = []
    _fixture_file_cursor = 0
    if MODE == 'replay':
        _fixture_file_events = [json.loads(line) for line in _fixture_file_path.read_text().splitlines()]
    def _fixture_wrap_file_tool(tool_name, marker, handler):
        def wrapped(args, **kwargs):
            global _fixture_file_cursor
            request = {'tool': tool_name, 'args': args}
            if MODE == 'record':
                _fixture_exchange('tool-begin:' + marker)
                output = handler(args, **kwargs)
                if not isinstance(output, str):
                    raise RuntimeError('fixture file tool returned a non-text result')
                with _fixture_file_path.open('a', encoding='utf-8') as log:
                    log.write(json.dumps({'request': request, 'output': output}, separators=(',', ':')) + '\n')
                _fixture_exchange('tool-end:' + marker)
                return output
            if _fixture_file_cursor >= len(_fixture_file_events):
                raise RuntimeError('fixture file replay exhausted')
            event = _fixture_file_events[_fixture_file_cursor]
            if event['request'] != request:
                raise RuntimeError('fixture file replay request differs')
            _fixture_file_cursor += 1
            Path(os.environ['ASB_FIXTURE_NATIVE_FILES_PROGRESS']).write_text(str(_fixture_file_cursor))
            _fixture_exchange('tool-end:' + marker)
            return event['output']
        return wrapped
    for _fixture_tool, _fixture_marker in (('write_file','write'),('read_file','read'),('patch','patch')):
        _fixture_entry = _fixture_registry._tools[_fixture_tool]
        _fixture_entry.handler = _fixture_wrap_file_tool(_fixture_tool, _fixture_marker, _fixture_entry.handler)
    @atexit.register
    def _fixture_finish_file_tools():
        if MODE == 'replay' and _fixture_file_cursor != len(_fixture_file_events):
            raise RuntimeError('fixture file replay left events unconsumed')
