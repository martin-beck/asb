// Keep the pinned CodeBuddy tool manager intact; wrap its native dispatch.
const original = 'async executeToolWithHooks(eA,el,ec,ed,eu,ep="legacy"){';
const replacement = `async executeToolWithHooks(eA,el,ec,ed,eu,ep="legacy"){return globalThis[Symbol.for('asb.workbuddy.nativeDispatch.v1')]({name:eA.name,input:ep==='parsed'?ec:this.getPlainToolInput(ec),session:ed?.context?.id,call:eu?.toolCall?.callId},()=>this.__asbOriginalToolDispatch(eA,el,ec,ed,eu,ep))}async __asbOriginalToolDispatch(eA,el,ec,ed,eu,ep="legacy"){`;
export function transformWorkBuddyDispatch(source) {
  if (typeof source !== 'string') throw new TypeError('WorkBuddy tool manager source must be text');
  const index=source.indexOf(original);
  if (index < 0 || source.indexOf(original,index+original.length) >= 0) throw new Error('WorkBuddy native tool manager layout is unsupported or ambiguous');
  return source.slice(0,index)+replacement+source.slice(index+original.length);
}
