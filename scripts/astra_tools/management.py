"""Black-box management-v2 HTTP client; no application services are simulated."""
import json
import ssl
import time
from urllib.error import HTTPError
from urllib.parse import urlencode
from urllib.request import HTTPSHandler, ProxyHandler, Request, build_opener
from uuid import uuid4


class ApiError(RuntimeError):
    def __init__(self, error):
        self.code = error['code']
        super().__init__(f"{self.code}: {error['message']}")


def decode_response(response, identity=None):
    assert response['version'] == 2, 'Unexpected management protocol'
    if identity is not None:
        assert response['id'] == identity, 'Management response identity mismatch'
    result = response['result']
    if 'Err' in result:
        raise ApiError(result['Err'])
    return result['Ok']


class ManagementHttp:
    def __init__(self, origin, ca):
        self.origin = origin
        self.token = None
        # Test traffic must never use the caller's ambient HTTP proxy.
        context = ssl.create_default_context(cafile=str(ca))
        self.opener = build_opener(ProxyHandler({}), HTTPSHandler(context=context))

    def http(self, method, path, data=None, *, token=True, headers=None, timeout=30):
        request_headers = {'Accept': 'application/json', 'Origin': self.origin}
        if token and self.token:
            request_headers['Authorization'] = f'Bearer {self.token}'
        if headers:
            request_headers.update(headers)
        payload = None
        if data is not None:
            payload = json.dumps(data).encode()
            request_headers['Content-Type'] = 'application/json'
        request = Request(self.origin + path, data=payload, headers=request_headers, method=method)
        try:
            response = self.opener.open(request, timeout=timeout)
        except HTTPError as error:
            response = error
        with response:
            raw = response.read(4 * 1024 * 1024 + 1)
            assert len(raw) <= 4 * 1024 * 1024, 'Oversized HTTP response'
            return response.status, json.loads(raw) if raw else None, response.headers

    def login(self, password):
        status, result, headers = self.http('POST', '/api/v2/login', {'password': password}, token=False)
        assert status == 200, f'Login returned HTTP {status}'
        assert headers.get('Cache-Control') == 'no-store'
        self.token = result['token']

    def request(self, body, agent=None):
        identity = str(uuid4())
        status, response, _ = self.http('POST', '/api/v2/request', {
            'version': 2, 'id': identity, 'agent_id': agent, 'body': body,
        }, timeout=120)
        assert status == 200, f'Management returned HTTP {status}: {response}'
        return decode_response(response, identity)

    def query(self, kind, agent=None, **data):
        query = {'type': kind}
        if data:
            query['data'] = data
        return self.request({'type': 'query', 'data': query}, agent)['data']

    def execute(self, agent, command, *, operation=None, **data):
        command = {'type': command, **({'data': data} if data else {})}
        return self.request({'type': 'execute', 'data': {
            'operation_id': operation or str(uuid4()), 'command': command,
        }}, agent)

    def draft(self, agent, command, **data):
        return self.request({'type': 'drafts', 'data': {'type': command, 'data': data}}, agent)

    def events(self, agent, after=None):
        """Read one actual SSE batch, closing the stream after bounded delivery."""
        query = {'agent_id': agent}
        if after is not None:
            query['after'] = after
        request = Request(self.origin + '/api/v2/events?' + urlencode(query), headers={
            'Authorization': f'Bearer {self.token}', 'Origin': self.origin,
            'Accept': 'text/event-stream',
        })
        with self.opener.open(request, timeout=15) as stream:
            assert stream.headers.get_content_type() == 'text/event-stream'
            size, lines = 0, []
            while size < 4 * 1024 * 1024:
                line = stream.readline(64 * 1024)
                if not line:
                    raise EOFError('SSE closed before an event')
                size += len(line)
                line = line.decode().rstrip('\r\n')
                if not line:
                    data = [line[5:].strip() for line in lines if line.startswith('data:')]
                    if data:
                        assert any(line.strip() == 'event: batch' for line in lines), lines
                        return decode_response(json.loads('\n'.join(data)))['data']
                    lines.clear()
                else:
                    lines.append(line)
            raise ValueError('Oversized SSE batch')


def eventually(probe, *, timeout=30, description='condition'):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = probe()
        if result:
            return result
        time.sleep(.1)
    raise TimeoutError(f'Timed out waiting for {description}')


def expect_error(code, action):
    try:
        action()
    except ApiError as error:
        assert error.code == code, f'Expected {code}, got {error}'
    else:
        raise AssertionError(f'Expected management error {code}')
