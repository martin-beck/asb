// Execute the actual report script against nodes parsed from its generated SVG.
const fs=require('node:fs');
const assert=require('node:assert/strict');
const html=fs.readFileSync(process.argv[2],'utf8');
const svgText=fs.readFileSync(process.argv[3],'utf8');
function element(attrs={}){
  const classes=new Set((attrs.class||'').split(/\s+/));
  const events=new Map();
  const dataset={};
  for(const [key,value] of Object.entries(attrs))if(key.startsWith('data-'))dataset[key.slice(5).replace(/-([a-z])/g,(_,c)=>c.toUpperCase())]=value;
  return {attrs,dataset,events,style:{},hidden:false,textContent:'',offsetWidth:120,offsetHeight:20,
    classList:{add:c=>classes.add(c),remove:c=>classes.delete(c),contains:c=>classes.has(c),toggle:(c,on)=>on?classes.add(c):classes.delete(c)},
    getAttribute:key=>attrs[key],setAttribute:(key,value)=>{attrs[key]=value;},
    addEventListener:(name,handler)=>{events.set(name,[...(events.get(name)||[]),handler]);},
    fire(name,event={}){for(const handler of events.get(name)||[])handler(event);},
    getBoundingClientRect:()=>({left:0,top:0,width:1200,height:600}),focus(){this.focused=true;},
    closest:()=>null,setPointerCapture(){this.capture=true;},hasPointerCapture(){return this.capture;},releasePointerCapture(){this.capture=false;}
  };
}
function attributes(tag){return Object.fromEntries([...tag.matchAll(/([\w-]+)="([^"]*)"/g)].map(match=>[match[1],match[2]]));}
const nodes=[...svgText.matchAll(/<(?:circle|polyline|rect|text)\b[^>]*>/g)].map(match=>element(attributes(match[0])));
const barSvg=process.argv[4]?fs.readFileSync(process.argv[4],'utf8'):'';
const bars=[...barSvg.matchAll(/<rect\b[^>]*class="bar"[^>]*>/g)].map(match=>element(attributes(match[0])));
nodes.push(...bars);
const points=nodes.filter(node=>node.classList.contains('series-point'));
const lines=nodes.filter(node=>node.classList.contains('series'));
assert.equal(points.length,2);
assert.deepEqual(points.map(point=>point.dataset.timestamp),['0','200']);
assert.deepEqual(points.map(point=>point.dataset.value),['1','3']);
assert.equal(lines.length,2,'null gap must split the generated line');
assert.ok(html.includes('data-dimension-benchmark="000-0"'));
assert.ok(svgText.includes('data-benchmark="000-0"'));
const svg=element(attributes(svgText.match(/<svg\b[^>]*>/)[0]));
svg.querySelectorAll=selector=>nodes.filter(node=>selector.split(',').some(part=>node.classList.contains(part.trim().slice(1))));
const tooltip=element(),reset=element(),expand=element(),close=element(),backdrop=element();
const stage=element();stage.querySelector=selector=>{assert.equal(selector,'svg');return svg;};
const chart=element();chart.querySelector=selector=>({'.chart-stage':stage,'.chart-tooltip':tooltip,'.chart-reset':reset,'.chart-expand':expand,'.chart-close':close,'.chart-backdrop':backdrop})[selector];
const body=element();const documentEvents=new Map();
global.document={body,querySelectorAll:selector=>{assert.equal(selector,'.interactive-chart');return[chart];},addEventListener:(name,handler)=>documentEvents.set(name,handler)};
const scripts=[...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].map(match=>match[1]);
const script=scripts.find(script=>script.includes('Nearest displayed observed sample'));
assert.ok(script,'test must execute the script embedded in the generated report');
eval(script);
if(barSvg){
  assert.equal(bars.length,1);
  assert.ok(Number(bars[0].getAttribute('height'))>0,'accepted negative scalar must have visible geometry');
  bars[0].fire('pointerenter',{clientX:100,clientY:100});
  assert.equal(tooltip.textContent,'bug-fix:default: -17.25','hover must preserve the original signed value');
  assert.ok(html.includes('data-value="-17.25"'),'actual HTML must embed the signed observation');
}
const pointer={clientX:Number(points[1].getAttribute('cx')),clientY:100};
lines[1].fire('pointerenter',pointer);
assert.ok(tooltip.textContent.includes('Nearest displayed observed sample: bug-fix:default: 3 @ 200 ms'),tooltip.textContent);
assert.ok(!tooltip.textContent.includes('undefined'));
let matrixUsed=false;
svg.getScreenCTM=()=>({inverse:()=>({observedX:Number(points[0].getAttribute('cx'))})});
svg.createSVGPoint=()=>({matrixTransform(matrix){matrixUsed=true;return{x:matrix.observedX};}});
lines[1].fire('pointermove',pointer);
assert.ok(matrixUsed,'browser screen matrix must account for letterboxing rather than only bounding-box scale');
assert.equal(tooltip.textContent,'Nearest displayed observed sample: bug-fix:default: 1 @ 0 ms');
points[0].fire('pointerenter',{clientX:100,clientY:100});
assert.equal(tooltip.textContent,'bug-fix:default: 1 @ 0 ms');
const original=svg.getAttribute('viewBox');
svg.fire('wheel',{deltaY:-1,clientX:600,clientY:200,preventDefault(){}});
assert.notEqual(svg.getAttribute('viewBox'),original);
const zoomed=svg.getAttribute('viewBox');
svg.fire('pointerdown',{clientX:600,clientY:200,pointerId:1,target:{closest:()=>null}});
svg.fire('pointermove',{clientX:400,clientY:180});
assert.notEqual(svg.getAttribute('viewBox'),zoomed);
svg.fire('pointerup',{pointerId:1});assert.ok(!svg.classList.contains('is-panning'));
reset.fire('click');assert.equal(svg.getAttribute('viewBox'),original);
expand.fire('click');assert.ok(chart.classList.contains('expanded'));assert.ok(body.classList.contains('chart-open'));assert.ok(close.focused);
documentEvents.get('keydown')({key:'Escape'});assert.ok(!chart.classList.contains('expanded'));assert.ok(expand.focused);
expand.fire('click');backdrop.fire('click');assert.ok(!chart.classList.contains('expanded'));
stage.fire('pointerleave');assert.ok(!tooltip.classList.contains('visible'));assert.ok(points.every(point=>!point.classList.contains('hovered')));
console.log('actual generated report interaction checks passed');
