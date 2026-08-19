const test = require('node:test');
const assert = require('node:assert');
const policy = require('../source/main.js');

const limits = { min_cells: 1, max_cells: 8 };

test('scale_up when avg qps exceeds threshold and below max_cells', () => {
  const out = policy({
    cells: [{ id: 'a', qps: 250.0, p99_ms: 12.0, mem_bytes: 1000, mem_budget_bytes: 4000 }],
    limits,
  });
  assert.strictEqual(out.action, 'scale_up');
  assert.strictEqual(out.target_cells, 2);
  assert.ok(out.reason.includes('avg qps'));
});

test('scale_down when avg qps below threshold and above min_cells', () => {
  const cell = { qps: 1.0, p99_ms: 5.0, mem_bytes: 1000, mem_budget_bytes: 4000 };
  const out = policy({
    cells: [
      { id: 'a', ...cell },
      { id: 'b', ...cell },
      { id: 'c', ...cell },
    ],
    limits,
  });
  assert.strictEqual(out.action, 'scale_down');
  assert.strictEqual(out.target_cells, 2);
  assert.ok(out.reason.includes('avg qps'));
});

test('hold when avg qps is within the band', () => {
  const out = policy({
    cells: [{ id: 'a', qps: 50.0, p99_ms: 8.0, mem_bytes: 1000, mem_budget_bytes: 4000 }],
    limits,
  });
  assert.strictEqual(out.action, 'hold');
  assert.strictEqual(out.target_cells, 1);
});

test('hold at max_cells even when avg qps exceeds the scale_up threshold', () => {
  const cell = { qps: 250.0, p99_ms: 12.0, mem_bytes: 1000, mem_budget_bytes: 4000 };
  const out = policy({
    cells: Array.from({ length: 8 }, (_, i) => ({ id: `c${i}`, ...cell })),
    limits,
  });
  assert.strictEqual(out.action, 'hold');
  assert.strictEqual(out.target_cells, 8);
});

test('hold at min_cells even when avg qps is below the scale_down threshold', () => {
  const out = policy({
    cells: [{ id: 'a', qps: 1.0, p99_ms: 5.0, mem_bytes: 1000, mem_budget_bytes: 4000 }],
    limits,
  });
  assert.strictEqual(out.action, 'hold');
  assert.strictEqual(out.target_cells, 1);
});

test('missing cells is a hold with an honest reason', () => {
  const out = policy({ cells: [], limits });
  assert.strictEqual(out.action, 'hold');
  assert.strictEqual(out.reason, 'no cells in snapshot');
});

test('undefined snapshot is a hold with an honest reason', () => {
  const out = policy(undefined);
  assert.strictEqual(out.action, 'hold');
  assert.strictEqual(out.reason, 'no cells in snapshot');
});
