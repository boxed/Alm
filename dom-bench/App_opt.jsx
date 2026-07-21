// Optimized React variant: the row is a React.memo component, so a state change
// (e.g. select) only re-renders the rows whose props actually changed. Props are
// kept referentially stable — the row object (unchanged on select), an
// `isSelected` boolean (flips for just two rows), and useCallback'd handlers —
// which is what lets memo skip the rest. Mirrors the alm Html.Lazy variant.
import React, { useState, useCallback, memo } from 'react';
import { createRoot } from 'react-dom/client';

let counter = 1;
const mk = (start, n) => { const a = []; for (let i = 0; i < n; i++) a.push({ id: start + i, label: 'item ' + (start + i) }); return a; };

const Row = memo(function Row({ row, isSelected, onSelect, onRemove }) {
  return (
    <tr className={isSelected ? 'danger' : ''}>
      <td className="col-md-1">{row.id}</td>
      <td className="col-md-4"><a onClick={() => onSelect(row.id)}>{row.label}</a></td>
      <td className="col-md-1"><a onClick={() => onRemove(row.id)}><span className="glyphicon glyphicon-remove"></span></a></td>
      <td className="col-md-6"></td>
    </tr>
  );
});

function App() {
  const [rows, setRows] = useState([]);
  const [selected, setSelected] = useState(0);
  const create = (n) => { const s = counter; counter += n; setRows(mk(s, n)); };
  const append = (n) => { const s = counter; counter += n; setRows(rs => rs.concat(mk(s, n))); };
  const update = () => setRows(rs => rs.map((r, i) => i % 10 === 0 ? { ...r, label: r.label + ' !!!' } : r));
  const swap = () => setRows(rs => { if (rs.length < 2) return rs; const a = rs.slice(); const t = a[1]; a[1] = a[a.length - 1]; a[a.length - 1] = t; return a; });
  const clear = () => { setRows([]); setSelected(0); };
  const onSelect = useCallback((id) => setSelected(id), []);
  const onRemove = useCallback((id) => setRows(rs => rs.filter(r => r.id !== id)), []);
  return (
    <div id="main">
      <button id="create" onClick={() => create(1000)}>create</button>
      <button id="create10k" onClick={() => create(10000)}>create10k</button>
      <button id="append" onClick={() => append(1000)}>append</button>
      <button id="update" onClick={update}>update</button>
      <button id="swap" onClick={swap}>swap</button>
      <button id="clear" onClick={clear}>clear</button>
      <table className="table"><tbody>
        {rows.map(r => (
          <Row key={r.id} row={r} isSelected={r.id === selected} onSelect={onSelect} onRemove={onRemove} />
        ))}
      </tbody></table>
    </div>
  );
}
createRoot(document.getElementById('app')).render(<App />);
