// Minimal DOM fixture for the actual embedded script, not a second filter implementation.
const assert = require('node:assert/strict');
const vm = require('node:vm');
function element(dataset = {}) {
  const events = new Map();
  const classes = new Set(['visible', 'hovered']);
  return {dataset, style: {}, hidden: false, checked: true,
    classList: {remove: value => classes.delete(value), contains: value => classes.has(value)},
    addEventListener: (name, callback) => events.set(name, callback),
    trigger: name => { assert.ok(events.has(name)); events.get(name)(); },
  };
}
const dimensions = ['benchmark', 'agent', 'provider', 'model', 'workload'];
const identities = [
  ['benchmark "one" <&>', 'agent-a', 'provider-a', 'model-a', 'workload-a'],
  ['benchmark-two', 'agent-b', 'provider-b', 'model-b', 'workload-b'],
  ['benchmark-replica-two', 'agent-a', 'provider-a', 'model-a', 'workload-a'],
];
const rows = identities.map(identity => element(Object.fromEntries(dimensions.map((name, index) =>
  ['dimension' + name[0].toUpperCase() + name.slice(1), identity[index]]))));
const inputs = dimensions.flatMap((dimension, index) => [...new Set(identities.map(row => row[index]))].map(value => {
  const input = element({group: dimension}); input.value = value; return input;
}));
function chart(labels) {
  const entries = labels.map(benchmark => element({benchmark}));
  const svg = element(); svg.viewBox = '0 0 900 460';
  const empty = element(); empty.hidden = true;
  const tooltip = element();
  return {entries, svg, empty, tooltip,
    querySelectorAll: selector => {
      if (selector === 'svg [data-benchmark]') return entries;
      if (selector === '.hovered') return entries;
      throw new Error(selector);
    },
    querySelector: selector => ({svg, '.chart-empty': empty, '.chart-tooltip': tooltip})[selector],
  };
}
const labels = identities.map(row => row[0]);
const flamegraphs = labels.map(flamegraphBenchmark => element({flamegraphBenchmark}));
const charts = [chart(labels), chart(labels), chart([labels[0], labels[2]])]; // bar, line, stacked timeline
const count = element();
const all = element({action: 'all'}), none = element({action: 'none'});
const document = {
  querySelectorAll: selector => ({'input[data-group]': inputs, '#results tr': rows,
    '.interactive-chart': charts, '[data-flamegraph-benchmark]': flamegraphs, 'button[data-action]': [all, none]})[selector],
  querySelector: selector => { assert.equal(selector, '#filter-count'); return count; },
};
vm.runInNewContext(reportScript, {document});
function expectVisible(expected) {
  const selected = new Set(expected);
  assert.deepEqual(rows.filter(row => !row.hidden).map(row => row.dataset.dimensionBenchmark), expected);
  assert.deepEqual(flamegraphs.filter(graph => !graph.hidden).map(graph => graph.dataset.flamegraphBenchmark), expected);
  for (const chart of charts) {
    for (const entry of chart.entries) {
      assert.equal(entry.style.display, selected.has(entry.dataset.benchmark) ? '' : 'none');
      assert.equal(entry.classList.contains('hovered'), false);
    }
    const hasData = chart.entries.some(entry => selected.has(entry.dataset.benchmark));
    assert.equal(chart.svg.style.display, hasData ? '' : 'none');
    assert.equal(chart.empty.hidden, hasData);
    assert.equal(chart.tooltip.classList.contains('visible'), false);
    assert.equal(chart.svg.viewBox, '0 0 900 460', 'filtering must not alter zoom/axes');
  }
  assert.ok(count.textContent.startsWith(`Showing ${expected.length} of 3 benchmarks.`));
}
expectVisible(labels);
for (let index = 0; index < dimensions.length; index++) {
  all.trigger('click');
  const input = inputs.find(input => input.dataset.group === dimensions[index] && input.value === identities[0][index]);
  input.checked = false; input.trigger('change');
  expectVisible(index === 0 ? [labels[1], labels[2]] : [labels[1]]);
  input.checked = true; input.trigger('change');
  expectVisible(labels);
}
// Combined dimensions use intersection, not union.
const agentA = inputs.find(input => input.dataset.group === 'agent' && input.value === 'agent-a');
const providerB = inputs.find(input => input.dataset.group === 'provider' && input.value === 'provider-b');
agentA.checked = false; agentA.trigger('change');
providerB.checked = false; providerB.trigger('change');
expectVisible([]);
none.trigger('click'); expectVisible([]);
all.trigger('click'); expectVisible(labels);
// A selected benchmark can have no data in one particular graph.
for (const input of inputs.filter(input => input.dataset.group === 'benchmark')) {
  input.checked = input.value === labels[1]; input.trigger('change');
}
expectVisible([labels[1]]);
