const test = require('node:test');
const assert = require('node:assert');
const policy = require('../source/main.js');

test('places least-assigned-first', () => {
  const out = policy({
    pending_tenants: [{ name: 't', replicas: 2 }],
    free_cells: ['cell-0', 'cell-1', 'cell-2'],
    assigned_counts: { 'cell-0': 3, 'cell-1': 0, 'cell-2': 1 },
  });
  assert.strictEqual(out.placements.length, 1);
  assert.strictEqual(out.placements[0].tenant, 't');
  assert.deepStrictEqual(out.placements[0].cells, ['cell-1', 'cell-2']);
});

test('places multiple tenants disjointly from the same free pool', () => {
  const out = policy({
    pending_tenants: [
      { name: 't1', replicas: 1 },
      { name: 't2', replicas: 1 },
    ],
    free_cells: ['cell-0', 'cell-1'],
    assigned_counts: {},
  });
  assert.strictEqual(out.placements.length, 2);
  const allCells = out.placements.flatMap((p) => p.cells);
  assert.strictEqual(
    new Set(allCells).size,
    allCells.length,
    'cells must be disjoint across placements'
  );
});

test('skips a tenant that cannot fit and names it in the reason', () => {
  const out = policy({
    pending_tenants: [{ name: 'too-big', replicas: 3 }],
    free_cells: ['cell-0', 'cell-1'],
    assigned_counts: {},
  });
  assert.strictEqual(out.placements.length, 0);
  assert.ok(out.reason.includes('too-big'));
});

test('an earlier tenant consuming the pool can starve a later one', () => {
  const out = policy({
    pending_tenants: [
      { name: 'first', replicas: 2 },
      { name: 'second', replicas: 1 },
    ],
    free_cells: ['cell-0', 'cell-1'],
    assigned_counts: {},
  });
  assert.strictEqual(out.placements.length, 1);
  assert.strictEqual(out.placements[0].tenant, 'first');
  assert.ok(out.reason.includes('second'));
});

test('is a pure function: identical input yields identical output', () => {
  const input = {
    pending_tenants: [{ name: 't', replicas: 1 }],
    free_cells: ['cell-0', 'cell-1'],
    assigned_counts: { 'cell-0': 1 },
  };
  const a = policy(JSON.parse(JSON.stringify(input)));
  const b = policy(JSON.parse(JSON.stringify(input)));
  assert.deepStrictEqual(a, b);
});

test('empty pending_tenants returns no placements with an honest reason', () => {
  const out = policy({ pending_tenants: [], free_cells: ['cell-0'], assigned_counts: {} });
  assert.strictEqual(out.placements.length, 0);
  assert.ok(out.reason.length > 0);
});

test('undefined input does not throw and returns no placements', () => {
  const out = policy(undefined);
  assert.strictEqual(out.placements.length, 0);
});

test('missing free_cells does not throw and skips every tenant', () => {
  const out = policy({ pending_tenants: [{ name: 't', replicas: 1 }] });
  assert.strictEqual(out.placements.length, 0);
  assert.ok(out.reason.includes('t'));
});
