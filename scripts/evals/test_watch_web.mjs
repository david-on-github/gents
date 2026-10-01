import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

function viewer(search='') {
  const html=fs.readFileSync(new URL('./watch-web.html',import.meta.url),'utf8');
  const script=html.split('<script>')[1].split('</script>')[0].replace('refresh();setInterval(refresh,3000);','');
  const elements=new Map();
  const context=vm.createContext({URLSearchParams,Intl,Date,location:{search},document:{
    getElementById(id){if(!elements.has(id))elements.set(id,{});return elements.get(id)},addEventListener(){}}});
  vm.runInContext(script,context);
  return (code)=>vm.runInContext(code,context);
}

test('completion uses requirements, not blended score; keeps setup and repair separate',()=>{
  const run=viewer();
  run(`globalThis.slot={live:{stages:{setup:{checks:[{check:'crew_spec_match',raw:{satisfied:90,total:100}}]},repair:{checks:[{check:'crew_spec_match',raw:{satisfied:100,total:100}}]}}},score_bp:9800};`);
  assert.match(run("scoreCell(slot,'setup')"),/90\.0%/);
  assert.match(run("scoreCell(slot,'repair')"),/100\.0%/);
  assert.equal(run("configurationScore({live:{},verdicts:[]},'setup')"),null);
  assert.match(run("scoreCell({live:{},verdicts:[{stage_id:'setup',check:'crew_spec_match',detail:'186 of 187 requirements met'}]},'setup')"),/99\.5%/);
});

test('object progress caps extras, preserves unknowns and escapes table content',()=>{
  const run=viewer();
  run(`globalThis.slot={key:'x',run:{key:'r',home:'/workstation-1',cases:[{case_id:'<script>',stages:['setup','repair']}]},case_id:'<script>',trial_index:0,state:'stopped',live:{documents:{AgentBehavior:10},schemas:[],tool_calls:70,failed_tool_calls:20},goal:[{collection:'AgentBehavior',min:7},{collection:'schemas',min:6}]};`);
  assert.equal(run('objectProgress(slot)'),100*7/13);
  const table=run('trialTable([slot])');
  assert.match(table,/<table/);assert.match(table,/Stopped/);assert.match(table,/10 \/ 7/);assert.match(table,/20 \/ 70/);
  assert.match(table,/&lt;script&gt;/);assert.doesNotMatch(table,/<script>/);
  assert.equal(run('objectProgress({...slot,live:{}})'),null);
});

test('active view follows current runs; selected batch retains only its explicit members',()=>{
  const run=viewer('?runs=old-a,old-b');
  run(`globalThis.items=[{key:'old-a',counts:{pass:16}},{key:'old-b',counts:{fail:16}},{key:'new-a',counts:{running:16}},{key:'new-b',counts:{running:16}}]`);
  assert.equal(run("JSON.stringify(selectedRunKeys(items,'active'))"),'["new-a","new-b"]');
  assert.equal(run("JSON.stringify(selectedRunKeys(items,'batch'))"),'["old-a","old-b"]');
  assert.equal(run("JSON.stringify(selectedRunKeys(items,'old-b'))"),'["old-b"]');
  run("items[2].counts={pass:16};items[3].counts={fail:16}");
  assert.equal(run("JSON.stringify(selectedRunKeys(items,'active'))"),'[]');
  assert.equal(run("JSON.stringify(selectedRunKeys(items,'batch'))"),'["old-a","old-b"]');
});
