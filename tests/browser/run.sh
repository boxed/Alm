#!/bin/sh
# Real-browser validation: compiles the test apps with alm AND the official
# elm compiler, runs the identical harness in headless Chrome, and prints
# both result sets. Requires: elm 0.19.1, Google Chrome, node.
set -e
cd "$(dirname "$0")"
ALM=${ALM:-../../target/release/alm}
CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

$ALM make src/Main.elm --output=app-alm.js
elm make src/Main.elm --output=app-elm.js > /dev/null
$ALM make src/App.elm --output=app2-alm.js
elm make src/App.elm --output=app2-elm.js > /dev/null

shim='<script>window.requestAnimationFrame=function(f){return window.setTimeout(function(){f(performance.now());},16);};window.cancelAnimationFrame=window.clearTimeout;</script>'
for who in alm elm; do
  printf '<!DOCTYPE html>\n<html><head><meta charset="utf-8"><title>t</title>%s</head><body><script src="app-%s.js"></script><script src="harness.js"></script></body></html>' "$shim" "$who" > element-$who.html
  printf '<!DOCTYPE html>\n<html><head><meta charset="utf-8"><title>t</title>%s</head><body><script src="app2-%s.js"></script><script src="harness-app.js"></script></body></html>' "$shim" "$who" > application-$who.html
done

extract() {
  python3 -c "
import sys, html, re
m = re.search(r'<pre id=\"results\">(.*?)</pre>', sys.stdin.read(), re.S)
print(html.unescape(m.group(1)) if m else 'NO RESULTS')
"
}

for who in alm elm; do
  echo "== Browser.element ($who):"
  "$CHROME" --headless=new --disable-gpu --virtual-time-budget=15000 --dump-dom "file://$PWD/element-$who.html" 2>/dev/null | extract | tail -1
done

for who in alm elm; do
  PAGE=application-$who.html node -e "
var http=require('http'),fs=require('fs'),path=require('path');
http.createServer(function(req,res){
  var n=req.url.split('?')[0];
  fs.readFile(path.join(process.cwd(), n.includes('.')?n:process.env.PAGE),function(e,d){
    if(e){res.writeHead(404);res.end();return;}
    res.writeHead(200,{'Content-Type':n.endsWith('.js')?'text/javascript':'text/html'});
    res.end(d);
  });
}).listen(8642);
" &
  SERVER=$!
  sleep 1
  echo "== Browser.application ($who):"
  "$CHROME" --headless=new --disable-gpu --virtual-time-budget=15000 --dump-dom "http://localhost:8642/" 2>/dev/null | extract | tail -1
  kill $SERVER
done
