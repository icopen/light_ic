import { type Principal } from '@dfinity/principal'
import { MockAgent } from './mock_agent'
import { loadWasm } from './instrumentation'
import fs from 'fs'
import { ReplicaContext } from './replica_context'
import { type Canister } from './canister'

export { LedgerHelper } from './ledger_helper'

export interface DeployOptions {
  initArgs?: any
  candid?: string
  id?: string
}

export class TestContext {
  replica: ReplicaContext

  compiled: Record<string, WebAssembly.Module>

  constructor () {
    this.replica = new ReplicaContext()
    this.compiled = {}
  }

  clean (): void {
    this.replica.clean()
  }

  getAgent (identity: Principal): MockAgent {
    const agent = new MockAgent(this.replica, identity)

    return agent
  }

  async deploy (filename: string, opts?: DeployOptions): Promise<Canister> {
    let module = this.compiled[filename]

    if (opts?.candid !== undefined) {
      if (fs.existsSync(opts.candid)) {
        opts.candid = fs.readFileSync(opts.candid).toString()
      }
    }

    if (module === undefined) {
      module = await loadWasm(filename)
      this.compiled[filename] = module
    }

    const result = await this.replica.install_canister(module, opts?.initArgs, opts?.candid, opts?.id)

    return result
  }

  // async deployWithId (filename: string, id: Principal, initArgs: any = null): Promise<Canister> {
  //   throw new Error('Not implemented')
  // }
}
