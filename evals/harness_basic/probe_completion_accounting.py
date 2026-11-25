#!/usr/bin/env python3
"""Compare loopback-provider usage with a real Yolop trajectory, without paid calls."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading


def probe(binary):
    calls = []

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def reply(self, body):
            encoded = json.dumps(body).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)

        def do_GET(self):
            self.reply({'object': 'list', 'data': [{'id': 'usage-probe', 'object': 'model'}]})

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            review = "Review the assistant's candidate final" in json.dumps(body.get('messages', []))
            prompt, completion = (900, 100) if review else (100, 20)
            usage = {'prompt_tokens': prompt, 'completion_tokens': completion, 'total_tokens': prompt + completion}
            calls.append({'purpose': 'completion_review' if review else 'generation', 'usage': usage})
            text = '{"state":"achieved","reason":"exact answer returned"}' if review else 'READY'
            response = {'id': f'probe-{len(calls)}', 'object': 'chat.completion', 'created': 1,
                        'model': 'usage-probe', 'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': text}, 'finish_reason': 'stop'}], 'usage': usage}
            if not body.get('stream'):
                self.reply(response)
                return
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.end_headers()
            chunk = {**response, 'object': 'chat.completion.chunk',
                     'choices': [{'index': 0, 'delta': {'role': 'assistant', 'content': text}, 'finish_reason': None}]}
            self.wfile.write(('data: ' + json.dumps(chunk) + '\n\n').encode())
            chunk['choices'] = [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}]
            self.wfile.write(('data: ' + json.dumps(chunk) + '\n\ndata: [DONE]\n\n').encode())
            self.wfile.flush()

    server = ThreadingHTTPServer(('127.0.0.1', 0), Provider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix='yolop-usage-probe-') as directory:
            root = Path(directory)
            config = root / 'config'
            config.mkdir()
            (config / 'settings.toml').write_text('worktrees = "off"\n')
            work = root / 'workspace'
            work.mkdir()
            env = {k: v for k, v in os.environ.items() if k in ('PATH', 'HOME', 'TMPDIR', 'RUSTUP_HOME', 'CARGO_HOME')}
            env.update(CUSTOM_API_KEY='local-only', CUSTOM_BASE_URL=f'http://127.0.0.1:{server.server_port}/v1')
            result = subprocess.run([str(Path(binary).resolve()), '--provider', 'custom', '--model', 'custom/usage-probe',
                                     '--config-dir', str(config), '--data-dir', str(root / 'data'),
                                     '--trajectory-out', str(root / 'trajectory.json'), '-C', str(work),
                                     '-p', 'Reply with exactly READY. Do not use tools.'], env=env,
                                    capture_output=True, text=True, timeout=60)
            if result.returncode:
                raise RuntimeError(result.stderr[-1500:])
            trajectory = json.loads((root / 'trajectory.json').read_text())
            if not any(c['purpose'] == 'completion_review' for c in calls):
                raise RuntimeError('completion review was not exercised')
            metrics = trajectory['final_metrics']
            observed = sum(c['usage']['total_tokens'] for c in calls)
            recorded = metrics['total_prompt_tokens'] + metrics['total_completion_tokens']
            return {'provider_calls': calls, 'served_total_tokens': observed,
                    'trajectory_recorded_tokens': recorded, 'unaccounted_tokens': observed - recorded,
                    'fully_accounted': observed == recorded, 'paid_requests': 0}
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--baseline', required=True)
    parser.add_argument('--candidate', required=True)
    parser.add_argument('--out', required=True, type=Path)
    args = parser.parse_args()
    data = {arm: probe(getattr(args, arm)) for arm in ('baseline', 'candidate')}
    args.out.write_text(json.dumps(data, indent=2))
    print(json.dumps(data, indent=2))


if __name__ == '__main__':
    main()
