// One visible-benchmark set controls both the table and every comparison chart.
// Keep original axes/layout and downloadable SVGs unchanged for reproducibility.
function installReportFilters(document) {
  const inputs = [...document.querySelectorAll('input[data-group]')];
  // Aggregate agent reports deliberately expose only the agent dimension;
  // derive active dimensions from the rendered controls instead of treating
  // absent provider/model/workload controls as empty selections.
  const dimensions = [...new Set(inputs.map(input => input.dataset.group))];
  const rows = [...document.querySelectorAll('#results tr')];
  const count = document.querySelector('#filter-count');
  const charts = [...document.querySelectorAll('.interactive-chart')].map(chart => ({
    entries: [...chart.querySelectorAll('svg [data-benchmark]')],
    svg: chart.querySelector('svg'),
    empty: chart.querySelector('.chart-empty'),
    tooltip: chart.querySelector('.chart-tooltip'),
    hovered: () => chart.querySelectorAll('.hovered'),
  }));

  function update() {
    const selected = Object.fromEntries(dimensions.map(dimension => [dimension,
      new Set(inputs.filter(input => input.dataset.group === dimension && input.checked)
        .map(input => input.value))]));
    const visible = new Set();
    for (const row of rows) {
      const show = dimensions.every(dimension => selected[dimension].has(
        row.dataset['dimension' + dimension[0].toUpperCase() + dimension.slice(1)]));
      row.hidden = !show;
      if (show) visible.add(row.dataset.dimensionBenchmark);
    }
    for (const graph of document.querySelectorAll('[data-flamegraph-benchmark]')) {
      graph.hidden = !visible.has(graph.dataset.flamegraphBenchmark);
    }
    for (const group of document.querySelectorAll('.distribution-group')) {
      group.hidden = !JSON.parse(group.dataset.members).some(label => visible.has(label));
    }
    for (const chart of charts) {
      let shown = 0;
      for (const entry of chart.entries) {
        const show = visible.has(entry.dataset.benchmark);
        // SVG does not consistently implement the HTML hidden attribute.
        entry.style.display = show ? '' : 'none';
        if (show) shown++;
      }
      chart.svg.style.display = shown ? '' : 'none';
      chart.empty.hidden = shown !== 0;
      chart.tooltip.classList.remove('visible');
      for (const item of chart.hovered()) item.classList.remove('hovered');
    }
    count.textContent = `Showing ${visible.size} of ${rows.length} benchmarks. Charts show only selected benchmarks; axes and downloadable SVGs retain the complete report scale and data.`;
  }

  for (const input of inputs) input.addEventListener('change', update);
  for (const button of document.querySelectorAll('button[data-action]')) {
    button.addEventListener('click', () => {
      for (const input of inputs) input.checked = button.dataset.action === 'all';
      update();
    });
  }
  update();
}
installReportFilters(document);
