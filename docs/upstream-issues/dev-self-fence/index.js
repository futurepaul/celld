export class Clock {
  constructor(ctx) { this.ctx = ctx; }
  async fetch(request) {
    const at = new URL(request.url).searchParams.get("at");
    if (at) await this.ctx.storage.setAlarm(Number(at));
    return Response.json({ alarm: await this.ctx.storage.getAlarm() });
  }
  async alarm() {}
}
export default { fetch: (request, env) => env.CLOCK.getByName("clock").fetch(request) };
