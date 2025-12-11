from simvm.sv import *
import json
import base64

unchanged = '/Users/sebastianwelsh/Development/sedaro/scf/results/20251211_152433/PTp6RL2JDwzJ9pSyJmhBcx.gnc.jsonl'
changed = '/Users/sebastianwelsh/Development/sedaro/scf/results/20251211_152450/PTp6RL2JDwzJ9pSyJmhBcx.gnc.jsonl'
other = '/Users/sebastianwelsh/Development/sedaro/scf/results/uq_run_0/PTp6RL2JDwzJ9pSyJmhBcx.gnc.jsonl'
other = '/Users/sebastianwelsh/Development/sedaro/scf/results/20251211_152451/PTp6RL2JDwzJ9pSyJmhBcx.gnc.jsonl'
other = '/Users/sebastianwelsh/Development/sedaro/scf/results/20251211_152452/PTp7n8dzjgctYtkldmSXb3.gnc.jsonl'
other = '/Users/sebastianwelsh/Development/sedaro/scf/results/uq_run_0/PTp7n8dzjgctYtkldmSXb3.gnc.jsonl'
other = '/Users/sebastianwelsh/Development/sedaro/scf/results/20251211_170700/PTp8Nk8rhvnqh8kwCc2pNd.gnc.jsonl'
other = '/Users/sebastianwelsh/Development/sedaro/scf/results/20251211_172459/PTp8jqMqyRlpBXYCTJWcF4.gnc.jsonl'
other = '/Users/sebastianwelsh/Development/sedaro/scf/results/20251211_172927/PTp8p58xzpcJNCTKJp6DW2.gnc.jsonl'


count = 0
errors = []
with open(other) as f:
    config = json.loads(f.readline())
    ty = Type(config['data']['type'])

    for line in f:
        try:
            data = json.loads(line)
            frame = ty.de(base64.b64decode(data['data']['frame']))
            count += 1
        except:
            continue
        errors.append(frame[24])

print(max(errors[int(len(errors)/2):]))

print(count)