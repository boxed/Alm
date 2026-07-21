// Shared PAINT-INCLUSIVE benchmark runner (matches the js-framework-benchmark
// spirit: user-visible latency from click to the painted frame). Each op is
// timed from just-before-click to a task scheduled after the next frame's paint
// (requestAnimationFrame -> setTimeout(0)). Sub-frame incremental ops therefore
// converge near one frame across all frameworks (they're paint-bound, not a real
// differentiator); bulk ops (create/append), whose work exceeds a frame, differ
// meaningfully. This is fair to frameworks that batch on rAF (elm) and those that
// commit synchronously (alm/react) alike — paint happens on the same frame.
(function () {
  function $(id){return document.getElementById(id);}
  function click(elOrId){const el=typeof elOrId==='string'?$(elOrId):elOrId; if(el)el.dispatchEvent(new MouseEvent('click',{bubbles:true,cancelable:true}));}
  function rows(){return document.querySelectorAll('table tr');}
  function firstSel(){return document.querySelector('table tr td:nth-child(2) a');}
  function firstRem(){return document.querySelector('table tr td:nth-child(3) a');}
  function afterPaint(){return new Promise(r=>requestAnimationFrame(()=>setTimeout(r,0)));}
  function med(a){a=a.slice().sort((x,y)=>x-y);return a[a.length>>1];}
  function ensure1k(){ click('clear'); click('create'); }
  async function timeOp(setup, act, warm, iters){
    for(let i=0;i<warm;i++){setup();await afterPaint();act();await afterPaint();}
    const ts=[];
    for(let i=0;i<iters;i++){setup();await afterPaint();const t=performance.now();act();await afterPaint();ts.push(performance.now()-t);}
    return med(ts);
  }
  const done=(txt)=>{const p=document.createElement('pre');p.id='results';p.textContent=txt;document.body.appendChild(p);document.title='DONE';};
  window.runBench = async function(){
    try{
      await afterPaint();
      const R={};
      R['create 1k']   = await timeOp(()=>click('clear'), ()=>click('create'), 3, 15);
      R['replace 1k']  = await timeOp(()=>click('create'), ()=>click('create'), 3, 15);
      R['create 10k']  = await timeOp(()=>click('clear'), ()=>click('create10k'), 2, 8);
      R['clear 1k']    = await timeOp(()=>click('create'), ()=>click('clear'), 3, 10);
      R['select']      = await timeOp(ensure1k, ()=>click(firstSel()), 3, 15);
      R['update 10th'] = await timeOp(ensure1k, ()=>click('update'), 3, 15);
      R['swap']        = await timeOp(ensure1k, ()=>click('swap'), 3, 15);
      R['remove']      = await timeOp(ensure1k, ()=>click(firstRem()), 3, 15);
      R['append 1k']   = await timeOp(()=>{click('clear');click('create10k');}, ()=>click('append'), 2, 8);
      done(JSON.stringify(R));
    }catch(e){done('ERROR '+(e&&e.stack||e));}
  };
})();
