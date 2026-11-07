import test from 'node:test';
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import { join } from 'node:path';
import { createHash } from 'node:crypto';
import { loadCandidates, prepareStore, saveCandidate } from '../assets/scripts/replay-discovery-store.mjs';

function fixture(t) {
  const parent=join(process.cwd(),'target/discovery-store-tests');fs.mkdirSync(parent,{recursive:true});
  const root=fs.mkdtempSync(join(parent,'store-'));t.after(()=>fs.rmSync(root,{recursive:true,force:true}));
  fs.mkdirSync(join(root,'.asb'));return {root, store:join(root,'.asb','exploration')};
}

test('private candidate data is deduplicated and reconstructed without storing source', t => {
  const {store}=fixture(t);prepareStore(store);
  const entries=[[JSON.stringify([{name:'bash',arguments:{command:'echo x'}},'observed']), 'expected']];
  const path=saveCandidate(store,entries);assert.equal(saveCandidate(store,entries),path);
  assert.equal(fs.statSync(path).mode & 0o077,0);
  assert.deepEqual(JSON.parse(fs.readFileSync(path,'utf8')).entries,entries);
  const loaded=loadCandidates(store);assert.equal(loaded.failures,0);assert.equal(loaded.candidates.length,1);assert.match(loaded.candidates[0].source,/normalizeToolResult/);
  assert.equal(fs.readdirSync(store).length,1);
});

test('corrupt, replaced and symlink candidates are never loaded or claimed saved', t => {
  const {root,store}=fixture(t);prepareStore(store);
  const entries=[[JSON.stringify([{name:'bash',arguments:{command:'x'}},'a']), 'b']];
  const bytes=Buffer.from(JSON.stringify({schema_version:1,entries}));
  const name=`candidate-${createHash('sha256').update(bytes).digest('hex')}.json`;
  const target=join(root,'target');fs.writeFileSync(target,bytes);
  fs.symlinkSync(target,join(store,name));
  assert.deepEqual(loadCandidates(store),{candidates:[],failures:1});
  assert.throws(()=>saveCandidate(store,entries));
  fs.unlinkSync(join(store,name));fs.writeFileSync(join(store,name),'corrupt');
  assert.deepEqual(loadCandidates(store),{candidates:[],failures:1});
  assert.throws(()=>saveCandidate(store,entries));
});

test('exploration directories may not redirect through symlinks', t => {
  const {root,store}=fixture(t);fs.symlinkSync(root,store);
  assert.throws(()=>prepareStore(store));
});
