import React, { useState } from 'react';
import { createRoot } from 'react-dom/client';
let counter = 1;
const mk = (start, n) => { const a = []; for (let i = 0; i < n; i++) a.push({ id: start + i, label: 'item ' + (start + i) }); return a; };
function App() {
  const [rows, setRows] = useState([]);
  const [selected, setSelected] = useState(0);
  const create = (n) => { const s = counter; counter += n; setRows(mk(s, n)); };
  const append = (n) => { const s = counter; counter += n; setRows(rs => rs.concat(mk(s, n))); };
  const update = () => setRows(rs => rs.map((r, i) => i % 10 === 0 ? { ...r, label: r.label + ' !!!' } : r));
  const swap = () => setRows(rs => { if (rs.length < 2) return rs; const a = rs.slice(); const t = a[1]; a[1] = a[a.length - 1]; a[a.length - 1] = t; return a; });
  const clear = () => { setRows([]); setSelected(0); };
  const remove = (id) => setRows(rs => rs.filter(r => r.id !== id));
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
          <tr key={r.id} className={r.id === selected ? 'danger' : ''}>
            <td className="col-md-1">{r.id}</td>
            <td className="col-md-4"><a onClick={() => setSelected(r.id)}>{r.label}</a></td>
            <td className="col-md-1"><a onClick={() => remove(r.id)}><span className="glyphicon glyphicon-remove"></span></a></td>
            <td className="col-md-6"></td>
          </tr>
        ))}
      </tbody></table>
    </div>
  );
}
createRoot(document.getElementById('app')).render(<App />);
