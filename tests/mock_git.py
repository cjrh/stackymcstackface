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
with (root / '.git' / 'mock-git.log').open('a') as log:
    log.write(json.dumps(args) + '\n')
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
    result = subprocess.run([real, *args])
    if result.returncode:
        sys.exit(result.returncode)
    # GitHub's PR heads follow successful pushes to the bare remote.
    for pr in state['prs']:
        head = subprocess.run([real, '--git-dir', state['bare'], 'rev-parse', '--verify',
                               'refs/heads/' + pr['headRefName']], capture_output=True, text=True)
        if head.returncode == 0:
            pr['headRefOid'] = head.stdout.strip()
    state_path.write_text(json.dumps(state))
    if state.get('uncertain_push'):
        sys.exit('connection lost after server accepted the push')
    sys.exit(0)
os.execv(real, [real, *args])
