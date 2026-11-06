"use strict";
// Finite safety abstraction, not a production-runtime or guest-execution proof.
// Two independent writers, one frozen lock identity, one immutable store slot.
const LIMIT = 12;
const init = {stage:[0,0],store:0,link:0,bad:false,crash:false,race:false,installed:false};
function must(ok, why) { if (!ok) throw new Error("package model: "+why); }
function valid(s) {
  must(s.stage.every(x => [0,1,2,3].includes(x)), "invalid staging state");
  must([0,1].includes(s.store), "unverified/wrong digest reached immutable store");
  must([0,1].includes(s.link), "invalid project link");
  must(s.link===0 || (s.link===1 && s.store===1), "link points outside verified store");
}
function next(s) {
  const out=[];
  for (let w=0;w<2;w++){
    const stage=s.stage[w];
    const stageTo=(n,extra={})=>({...s,...extra,stage:s.stage.map((x,i)=>i===w?n:x)});
    if(stage===0) {
      out.push(["download-matching-"+w,stageTo(1)]);
      out.push(["download-tampered-"+w,stageTo(2)]);
    }
    if(stage===1) out.push(["verify-"+w,stageTo(3)]);
    if(stage===2) out.push(["reject-hash-mismatch-"+w,stageTo(0,{bad:true})]);
    if(stage===3 && s.store===0) out.push(["atomic-store-promote-"+w,stageTo(0,{store:1})]);
    if(stage===3 && s.store===1) out.push(["no-overwrite-"+w,stageTo(0,{race:true})]);
    if(stage!==0) out.push(["crash-drop-staging-"+w,stageTo(0,{crash:true})]);
  }
  if(s.store===1 && s.link===0) out.push(["install-frozen", {...s,link:1,installed:true}]);
  return out;
}
function explore() {
  const queue=[[init,0]], visited=new Set([JSON.stringify(init)]), traces=new Map();
  let transitions=0;
  while(queue.length) {
    const [state,depth]=queue.shift();
    valid(state);
    if(depth===LIMIT) continue;
    for(const [action,target] of next(state)) {
      valid(target); transitions++;
      const key=JSON.stringify(target);
      if(!visited.has(key)) { visited.add(key); queue.push([target,depth+1]); traces.set(key,action); }
    }
  }
  const states=[...visited].map(x=>JSON.parse(x));
  for(const flag of ["bad","crash","race","installed"])
    must(states.some(x=>x[flag]===true), "missing negative/positive witness: "+flag);
  // Mutation checks make the safety oracle observable even when transitions are hardened.
  for(const forged of [{...init,store:2},{...init,link:1},{...init,stage:[4,0]}]) {
    let rejected=false;
    try {valid(forged);} catch {rejected=true;}
    must(rejected,"unsafe mutation was not detected");
  }
  return {states:visited.size,transitions,bound:LIMIT,writers:2,witnesses:["mismatch","crash","race","frozen-install"],claim:"bounded-source-model-only"};
}
const result=explore();
if(typeof console!=="undefined") console.log(JSON.stringify(result));
