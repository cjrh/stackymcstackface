#!/usr/bin/env python3
"""Use real Git except for GitHub URL identity, which is isolated from networking."""
import json
import os
import pathlib
import subprocess
import sys

args = sys.argv[1:]
real = os.environ['REAL_GIT']
root = pathlib.Path(os.environ['MOCK_REPO'])
state_path = root / '.git' / 'mock-gh.json'
state = json.loads(state_path.read_text())
if args[:2] == ['remote', 'get-url']:
    print('https://github.com/other/repo' if state.get('wrong_push_url') and '--push' in args else 'https://github.com/owner/repo')
    sys.exit(0)
if args and args[0] == 'push':
    with (root / '.git' / 'mock-push.log').open('a') as log:
        log.write(json.dumps(args) + '\n')
    if state.get('race_push_branch'):
        subprocess.run([real, '--git-dir', state['bare'], 'update-ref', 'refs/heads/' + state['race_push_branch'], state['base']], check=True)
    if state.get('reject_push'):
        sys.exit('simulated rejected push')
    if state.get('uncertain_push'):
        subprocess.run([real, *args], check=True)
        sys.exit('connection lost after server accepted the push')
os.execv(real, [real, *args])
