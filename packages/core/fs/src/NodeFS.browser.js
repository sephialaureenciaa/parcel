// @flow
import type {FileSystem} from '@parcel/types-internal';

// $FlowFixMe[prop-missing] handled by the throwing constructor
export class NodeFS implements FileSystem {
  constructor() {
    throw new Error("NodeFS isn't available in the browser");
  }
}
