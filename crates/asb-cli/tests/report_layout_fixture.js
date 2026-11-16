// Exercise the report's shared layout transform against a tiny DOM model.
// This catches accidental regression to eagerly expanded tables/figures
// without pulling a browser or a DOM package into offline test dependencies.
const fs=require('node:fs');
const assert=require('node:assert/strict');

function node(tagName,{dataset={},classes=[]}={}){
  const children=[];
  const classSet=new Set(classes);
  return {tagName:tagName.toUpperCase(),dataset,children,parentElement:null,textContent:'',className:'',
    classList:{add:value=>classSet.add(value),contains:value=>classSet.has(value)},
    append(...entries){for(const entry of entries){
      if(entry.parentElement){const index=entry.parentElement.children.indexOf(entry);if(index>=0)entry.parentElement.children.splice(index,1);}
      children.push(entry);entry.parentElement=this;
    }},
    insertBefore(entry,before){
      if(entry.parentElement){const index=entry.parentElement.children.indexOf(entry);if(index>=0)entry.parentElement.children.splice(index,1);}
      const index=children.indexOf(before);children.splice(index<0?children.length:index,0,entry);entry.parentElement=this;
    },
    closest(selector){
      let current=this;
      while(current){
        if(selector==='details'&&current.tagName==='DETAILS')return current;
        if(selector==='.table-wrap'&&current.classList&&current.classList.contains('table-wrap'))return current;
        current=current.parentElement;
      }
      return null;
    },
  };
}

const section=node('section');
const wrap=node('div',{classes:['table-wrap']});
const table=node('table');
wrap.append(table);section.append(wrap);
const grid=node('div',{classes:['charts']});
const perf=node('figure',{classes:['interactive-chart'],dataset:{collector:'perf'}});
const pressure=node('figure',{classes:['interactive-chart'],dataset:{collector:'Kernel pressure'}});
grid.append(perf,pressure);
const documentEvents=new Map();
global.document={
  body:node('body'),
  createElement:tag=>node(tag),
  addEventListener:(name,handler)=>documentEvents.set(name,handler),
  querySelectorAll:selector=>({
    table:[table],
    '.charts':[grid],
    '.interactive-chart':[],
    'input[data-highlight-graph]':[],
    '[data-highlight-row]':[],
    '.flamegraph-chart':[],
  })[selector]||[],
};

eval(fs.readFileSync(process.argv[2],'utf8'));

assert.equal(section.children.length,1);
const folded=section.children[0];
assert.equal(folded.tagName,'DETAILS');
assert.notEqual(folded.open,true,'details must be closed by default');
assert.equal(folded.children[0].tagName,'SUMMARY');
assert.equal(folded.children[1],wrap);
assert.equal(grid.children.length,2);
for(const group of grid.children){
  assert.equal(group.tagName,'DETAILS');
  assert.notEqual(group.open,true,'collector group must be closed by default');
  assert.equal(group.children[0].tagName,'SUMMARY');
  assert.equal(group.children[1].className,'charts collector-charts');
  assert.equal(group.children[1].children.length,1);
}
console.log('report folding and collector grouping checks passed');
