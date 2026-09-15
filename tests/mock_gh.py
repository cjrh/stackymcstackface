#!/usr/bin/env python3
"""Deterministic GitHub double. All state and invocation logs live in a temp repo."""
import json
import os
import pathlib
import sys

root = pathlib.Path(os.environ['MOCK_REPO'])
path = root / '.git' / 'mock-gh.json'
state = json.loads(path.read_text())
args = sys.argv[1:]
with (root / '.git' / 'mock-gh.log').open('a') as log:
    log.write(json.dumps(args) + '\n')

def emit(data):
    print(json.dumps(data))
    sys.exit(0)

def save():
    path.write_text(json.dumps(state))

def current(pr):
    pr = dict(pr)
    merged_heads = {p['headRefName'] for p in state['prs'] if p['number'] in state['merged']}
    if pr['baseRefName'] in merged_heads:
        pr['baseRefName'] = 'main'
    return pr

if args[:2] == ['repo', 'view']:
    emit({'nameWithOwner':'owner/repo','defaultBranchRef':{'name':'main'},'isFork':False,
          'parent':None,'deleteBranchOnMerge':True,'viewerPermission':'WRITE'})
if args[:2] == ['pr', 'list']:
    result = [current(p) for p in state['prs'] if p['number'] not in state['merged']]
    if state.get('late_alias_after'):
        state['list_calls'] = state.get('list_calls', 0) + 1
        save()
        if state['list_calls'] >= state['late_alias_after']:
            alias = dict(state['prs'][-1])
            alias.update(number=4, headRefName='alias', baseRefName='main')
            result.append(alias)
    if state.get('truncated'):
        result = result * 1000
        result = result[:1000]
    emit(result)
if args[:2] == ['pr', 'view']:
    number = int(args[2])
    pr = current(next(p for p in state['prs'] if p['number'] == number))
    base_oid = state['base'] if pr['baseRefName'] == 'main' else next(p['headRefOid'] for p in state['prs'] if p['headRefName'] == pr['baseRefName'])
    blocked = state.get('blocked') == number or (state.get('partial_block') == number and state['merged'])
    pr.update({'state':'MERGED' if number in state['merged'] else 'OPEN',
               'isDraft':state.get('draft') == number,'mergeable':'UNKNOWN' if state.get('unknown') == number else 'MERGEABLE',
               'mergeStateStatus':'BLOCKED' if blocked else 'CLEAN','reviewDecision':'APPROVED',
               'baseRefOid':base_oid,'statusCheckRollup':[]})
    if state.get('failed_check') == number:
        pr['statusCheckRollup'] = [{'__typename':'CheckRun','status':'COMPLETED','conclusion':'FAILURE'}]
    if state.get('changed_head') == number:
        pr['headRefOid'] = 'f' * 40
    emit(pr)
if args[0] == 'api':
    if 'graphql' in args:
        number = int(next(a.split('=',1)[1] for a in args if a.startswith('number=')))
        emit({'data':{'repository':{'pullRequest':{'isMergeQueueEnabled':state.get('queue') == number,'isInMergeQueue':False}}}})
    endpoint = next((a for a in args if a.startswith('repos/')), '')
    if '/rules/branches/' in endpoint:
        if state.get('rules_error'):
            sys.exit('rules unavailable')
        emit([[{'type':'merge_queue'}]] if state.get('rule_queue') else [[]])
    if endpoint == 'repos/owner/repo':
        emit({'allow_merge_commit':not state.get('no_merge_commits'),'delete_branch_on_merge':not state.get('no_delete'),'default_branch':'main'})
    if endpoint.endswith('/merge'):
        number = int(endpoint.split('/')[-2])
        if state.get('merge_failure') == number:
            sys.exit('merge rejected')
        assert 'merge_method=merge' in args
        pr = next(p for p in state['prs'] if p['number'] == number)
        assert 'sha=' + pr['headRefOid'] in args
        state['merged'].append(number)
        save()
        emit({'merged':True,'message':'merged'})
sys.exit('unexpected gh command: ' + repr(args))
