#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import {createRequire} from 'node:module';
const require=createRequire(path.resolve('.repotunnel-tmp/website-manual-ui-review/browser-tools/package.json'));
const {chromium}=require('playwright-core');
const sharp=require('sharp');
const root='.repotunnel-tmp/website-manual-ui-review';
const out=path.join(root,'captures');fs.mkdirSync(out,{recursive:true});
const origin='http://127.0.0.1:3010';
const routes=[];
function walk(dir){for(const entry of fs.readdirSync(dir,{withFileTypes:true})){const f=path.join(dir,entry.name);if(entry.isDirectory())walk(f);else if(f.endsWith('.html'))routes.push('/'+path.relative('dist',f).replaceAll(path.sep,'/').replace(/index\.html$/,''));}}
walk('dist');routes.sort();
const priority=['/','/product/','/product/chatgpt-extension/','/solutions/','/solutions/chatgpt-local-projects/','/docs/','/docs/getting-started/what-is-repotunnel/','/docs/getting-started/app-tour/','/docs/connections/direct-https/','/install/','/downloads/','/security/','/privacy/','/community/','/changelog/','/changelog/0.4.1/','/404.html'];
routes.sort((a,b)=>(priority.indexOf(a)<0?999:priority.indexOf(a))-(priority.indexOf(b)<0?999:priority.indexOf(b))||a.localeCompare(b));
if(routes.length!==77)throw new Error('Review the changed route inventory before capturing');
const browser=await chromium.connectOverCDP('http://127.0.0.1:44085');
const page=browser.contexts().flatMap(c=>c.pages()).find(p=>p.url().startsWith(origin+'/'));
if(!page)throw new Error('Local QA tab missing');
const cdp=await page.context().newCDPSession(page);
const info=await cdp.send('Target.getTargetInfo');
if(info.targetInfo.targetId!=='571DA24A499FD76F6D4D19D76D6BAFAF')throw new Error('Unexpected QA tab');
const records=[],failures=[];
const save=()=>fs.writeFileSync(path.join(root,'capture-manifest.json'),JSON.stringify({status:'capturing',records,failures},null,2)+'\n');
try{
 await page.bringToFront();await cdp.send('Emulation.setFocusEmulationEnabled',{enabled:true});
 await page.evaluate(()=>localStorage.setItem('repotunnel-theme','dark'));
 for(let index=0;index<routes.length;index++){
  const route=routes[index],record={index,route,theme:'dark',variants:[],sheets:[],manualStatus:'not-yet-reviewed'};
  for(const width of [1440,375]){
   await page.setViewportSize({width,height:900});const response=await page.goto(origin+route,{waitUntil:'load'});if(response?.status()!==200)throw new Error('Non-200 review route: '+route);await page.addStyleTag({content:'html{scrollbar-width:none!important}'});await page.evaluate(()=>document.fonts.ready);
   const maxY=await page.evaluate(()=>Math.max(0,document.documentElement.scrollHeight-innerHeight));
   const positions=[...new Set([0,...Array.from({length:Math.ceil(maxY/650)},(_,i)=>Math.min((i+1)*650,maxY)),maxY])];let footerReached=false;
   for(const y of positions){
    await page.evaluate(y=>{document.documentElement.style.scrollBehavior='auto';window.scrollTo({top:y,behavior:'instant'});},y);await page.waitForTimeout(70);
    footerReached ||= await page.evaluate(()=>{const f=document.querySelector('footer')?.getBoundingClientRect();return !!f&&f.top<innerHeight&&f.bottom>0;});
   }
   await page.waitForTimeout(250);await page.evaluate(()=>window.scrollTo({top:0,behavior:'instant'}));await page.waitForTimeout(250);
   const filename=String(index).padStart(2,'0')+'-'+width+'.png';
   await page.screenshot({path:path.join(out,filename),fullPage:true,animations:'disabled',caret:'hide'});
   const state=await page.evaluate(()=>{const main=document.querySelector('main')?.cloneNode(true);main?.querySelectorAll('aside,nav,.docs-sidebar').forEach(e=>e.remove());return {title:document.title,headings:[...document.querySelectorAll('main h1,main h2,main h3')].map(e=>({level:e.tagName,text:e.textContent.trim()})),mainText:main?.innerText||main?.textContent||'',height:document.documentElement.scrollHeight,overflow:document.documentElement.scrollWidth>innerWidth+1};});
   record.variants.push({width,filename,scrollStops:positions.length,footerReached,height:state.height,overflow:state.overflow});
   if(width===1440){record.title=state.title;record.headings=state.headings;record.mainText=state.mainText;}
   if(!footerReached||state.overflow)failures.push({route,width,footerReached,overflow:state.overflow});
  }
  const desktop=await sharp(path.join(out,record.variants[0].filename)).resize({width:600}).png().toBuffer();
  const mobile=await sharp(path.join(out,record.variants[1].filename)).resize({width:375}).png().toBuffer();
  const dm=await sharp(desktop).metadata(),mm=await sharp(mobile).metadata();
  const count=Math.max(Math.ceil(dm.height/1200),Math.ceil(mm.height/2400));
  for(let s=0;s<count;s++){
   const pieces=[];
   const label='<svg width="1374" height="38"><rect width="100%" height="100%" fill="#ffffff"/><text x="10" y="25" font-size="18" fill="#111111">'+String(index).padStart(2,'0')+' '+route.replaceAll('&','&amp;')+' — desktop | mobile '+(s+1)+'/'+count+'</text></svg>';
   pieces.push({input:Buffer.from(label),left:0,top:0});
   const dy=s*1200;if(dy<dm.height)pieces.push({input:await sharp(desktop).extract({left:0,top:dy,width:600,height:Math.min(1200,dm.height-dy)}).toBuffer(),left:0,top:38});
   for(let k=0;k<2;k++){const my=s*2400+k*1200;if(my<mm.height)pieces.push({input:await sharp(mobile).extract({left:0,top:my,width:375,height:Math.min(1200,mm.height-my)}).toBuffer(),left:612+k*381,top:38});}
   const sheet=String(index).padStart(2,'0')+'-sheet-'+s+'.webp';
   await sharp({create:{width:1374,height:1238,channels:3,background:'#d5d5d5'}}).composite(pieces).webp({quality:72}).toFile(path.join(out,sheet));
   record.sheets.push(sheet);
  }
  records.push(record);save();console.log(JSON.stringify({index,route,sheets:record.sheets,footerCoverage:record.variants.filter(v=>v.footerReached).length}),flush=>flush);
 }
}catch(e){failures.push({type:'capture',message:e.message});process.exitCode=1;}
finally{
 for(const[name,fn]of [['focus',()=>cdp.send('Emulation.setFocusEmulationEnabled',{enabled:false})],['metrics',()=>cdp.send('Emulation.clearDeviceMetricsOverride')],['home',()=>page.goto(origin+'/')],['detach',()=>cdp.detach()],['disconnect',()=>browser.close()]]){
  let timer;try{await Promise.race([fn(),new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error('Cleanup timeout')),10000);})]);}catch(e){failures.push({type:'cleanup',step:name,message:e.message});process.exitCode=1;}finally{clearTimeout(timer);}
 }
 fs.writeFileSync(path.join(root,'capture-manifest.json'),JSON.stringify({status:failures.length?'failed':'captured',records,failures},null,2)+'\n');
 console.log(JSON.stringify({capturedPages:records.length,variants:records.length*2,failures}));
}
