// usage: node act.mjs out.png "click:x,y" "wait:secs" "key:..." ...   (page must already be open via shot.mjs's chrome; reuses first page)
import fs from 'node:fs';
const [out, ...steps] = process.argv.slice(2);
const base='http://127.0.0.1:9333';
const page=(await (await fetch(base+'/json/list')).json()).find(t=>t.type==='page');
const ws=new WebSocket(page.webSocketDebuggerUrl); await new Promise(r=>ws.addEventListener('open',r));
let id=0; const pend=new Map();
ws.addEventListener('message',m=>{const d=JSON.parse(m.data); if(d.id&&pend.has(d.id)){pend.get(d.id)(d);pend.delete(d.id);}});
const send=(method,params={})=>new Promise(r=>{const i=++id;pend.set(i,r);ws.send(JSON.stringify({id:i,method,params}));});
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
for (const st of steps){
  const [k,v]=[st.slice(0,st.indexOf(':')),st.slice(st.indexOf(':')+1)];
  if(k==='wait') await sleep(Number(v)*1000);
  else if(k==='click'||k==='dblclick'){const [x,y]=v.split(',').map(Number);
    await send('Input.dispatchMouseEvent',{type:'mouseMoved',x,y}); await sleep(200);
    for(let n=0;n<(k==='dblclick'?2:1);n++){
    await send('Input.dispatchMouseEvent',{type:'mousePressed',x,y,button:'left',clickCount:n+1}); await sleep(80);
    await send('Input.dispatchMouseEvent',{type:'mouseReleased',x,y,button:'left',clickCount:n+1}); await sleep(120);} }
  else if(k==='type'){ for(const ch of v){ await send('Input.dispatchKeyEvent',{type:'keyDown',text:ch,key:ch}); await send('Input.dispatchKeyEvent',{type:'keyUp',key:ch}); await sleep(60);} }
  else if(k==='ctrl'){ const c=v.toUpperCase(); await send('Input.dispatchKeyEvent',{type:'rawKeyDown',modifiers:2,key:v,code:'Key'+c,windowsVirtualKeyCode:c.charCodeAt(0)}); await send('Input.dispatchKeyEvent',{type:'keyUp',modifiers:2,key:v,code:'Key'+c,windowsVirtualKeyCode:c.charCodeAt(0)}); }
  else if(k==='key'){ await send('Input.dispatchKeyEvent',{type:'keyDown',key:v,code:v,windowsVirtualKeyCode:v==='Enter'?13:0}); await send('Input.dispatchKeyEvent',{type:'keyUp',key:v,code:v}); }
}
const shot=await send('Page.captureScreenshot',{format:'png'});
fs.writeFileSync(out,Buffer.from(shot.result.data,'base64')); ws.close(); process.exit(0);
