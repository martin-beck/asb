document.querySelectorAll('.interactive-chart').forEach(chart=>{
  const stage=chart.querySelector('.chart-stage');
  const svg=stage.querySelector('svg');
  const tooltip=chart.querySelector('.chart-tooltip');
  const reset=chart.querySelector('.chart-reset');
  const expand=chart.querySelector('.chart-expand');
  const close=chart.querySelector('.chart-close');
  const backdrop=chart.querySelector('.chart-backdrop');
  const original=svg.getAttribute('viewBox').split(/\s+/).map(Number);
  let view={x:original[0],y:original[1],w:original[2],h:original[3]};
  let drag=null;
  const interactive=[...svg.querySelectorAll('.bar,.series,.series-point')];
  const clearHover=()=>interactive.forEach(item=>item.classList.remove('hovered'));
  const positionTooltip=(event)=>{
    const bounds=stage.getBoundingClientRect();
    const left=Math.min(Math.max(8,event.clientX-bounds.left+12),Math.max(8,bounds.width-tooltip.offsetWidth-8));
    const top=Math.min(Math.max(8,event.clientY-bounds.top-42),Math.max(8,bounds.height-tooltip.offsetHeight-8));
    tooltip.style.left=`${left}px`;tooltip.style.top=`${top}px`;
  };
  const show=(item,event)=>{
    clearHover();
    let observed=item;
    let prefix='';
    if(item.classList.contains('series')){
      const bounds=svg.getBoundingClientRect();
      let x=view.x+(event.clientX-bounds.left)/bounds.width*view.w;
      // The browser matrix also accounts for preserveAspectRatio letterboxing
      // in enlarged charts; bounding-box scaling alone can select the wrong sample.
      if(svg.createSVGPoint&&svg.getScreenCTM()){
        const point=svg.createSVGPoint();point.x=event.clientX;point.y=event.clientY;
        x=point.matrixTransform(svg.getScreenCTM().inverse()).x;
      }
      const samples=[...svg.querySelectorAll('.series-point')].filter(point=>point.dataset.seriesIndex===item.dataset.seriesIndex);
      observed=samples.reduce((nearest,point)=>!nearest||Math.abs(Number(point.getAttribute('cx'))-x)<Math.abs(Number(nearest.getAttribute('cx'))-x)?point:nearest,null);
      if(!observed){tooltip.textContent='No observed sample is available for this series.';tooltip.classList.add('visible');positionTooltip(event);return;}
      prefix='Nearest displayed observed sample: ';
    }
    observed.classList.add('hovered');
    if(observed.dataset.status==='unavailable'){
      tooltip.textContent=observed.dataset.label+': unavailable';
      tooltip.classList.add('visible');positionTooltip(event);return;
    }
    if(observed.classList.contains('series-point')){
      for(const line of svg.querySelectorAll('.series'))if(line.dataset.seriesIndex===observed.dataset.seriesIndex)line.classList.add('hovered');
    }
    const time=observed.dataset.timestamp;
    const index=observed.dataset.sampleIndex;
    const location=time!==undefined?' @ '+time+' ms':index!==undefined?' @ sample '+index:'';
    tooltip.textContent=prefix+observed.dataset.label+': '+observed.dataset.value+location;
    tooltip.classList.add('visible');
    positionTooltip(event);
  };
  interactive.forEach(item=>{item.addEventListener('pointerenter',event=>show(item,event));if(item.classList.contains('series'))item.addEventListener('pointermove',event=>{if(!drag)show(item,event)});});
  stage.addEventListener('pointermove',event=>{if(!drag)positionTooltip(event)});
  stage.addEventListener('pointerleave',()=>{clearHover();tooltip.classList.remove('visible')});
  const applyView=()=>svg.setAttribute('viewBox',`${view.x} ${view.y} ${view.w} ${view.h}`);
  const setExpanded=expanded=>{
    chart.classList.toggle('expanded',expanded);
    document.body.classList.toggle('chart-open',expanded);
    expand.hidden=expanded;close.hidden=!expanded;
    if(expanded)close.focus();else expand.focus();
  };
  svg.addEventListener('wheel',event=>{
    event.preventDefault();
    const bounds=svg.getBoundingClientRect();
    const factor=event.deltaY<0?.85:1.15;
    const mx=view.x+(event.clientX-bounds.left)/bounds.width*view.w;
    const my=view.y+(event.clientY-bounds.top)/bounds.height*view.h;
    const w=Math.min(original[2],Math.max(original[2]/8,view.w*factor));
    const h=Math.min(original[3],Math.max(original[3]/8,view.h*factor));
    view.x=Math.max(0,Math.min(original[2]-w,mx-(mx-view.x)*w/view.w));
    view.y=Math.max(0,Math.min(original[3]-h,my-(my-view.y)*h/view.h));
    view.w=w;view.h=h;applyView();
  },{passive:false});
  svg.addEventListener('pointerdown',event=>{
    if(event.target.closest('.bar,.series-point'))return;
    drag={x:event.clientX,y:event.clientY,view:{...view}};svg.classList.add('is-panning');svg.setPointerCapture(event.pointerId);
  });
  svg.addEventListener('pointermove',event=>{
    if(!drag)return;
    const bounds=svg.getBoundingClientRect();
    view.x=Math.max(0,Math.min(original[2]-view.w,drag.view.x-(event.clientX-drag.x)/bounds.width*view.w));
    view.y=Math.max(0,Math.min(original[3]-view.h,drag.view.y-(event.clientY-drag.y)/bounds.height*view.h));
    applyView();
  });
  const stopDrag=event=>{if(!drag)return;drag=null;svg.classList.remove('is-panning');if(svg.hasPointerCapture(event.pointerId))svg.releasePointerCapture(event.pointerId)};
  svg.addEventListener('pointerup',stopDrag);svg.addEventListener('pointercancel',stopDrag);
  reset.addEventListener('click',()=>{view={x:original[0],y:original[1],w:original[2],h:original[3]};applyView()});
  expand.addEventListener('click',()=>setExpanded(true));
  close.addEventListener('click',()=>setExpanded(false));
  backdrop.addEventListener('click',()=>setExpanded(false));
  document.addEventListener('keydown',event=>{if(event.key==='Escape'&&chart.classList.contains('expanded'))setExpanded(false)});
});
