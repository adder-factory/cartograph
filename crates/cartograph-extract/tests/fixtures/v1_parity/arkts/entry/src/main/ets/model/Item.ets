import { helper, Logger, ConsoleLogger } from '../common/Utils';

export interface Identifiable {
  id: number;
}

export class BaseModel {
  protected logger: Logger = new ConsoleLogger();

  describe(): string {
    return 'base';
  }
}

export class Item extends BaseModel implements Identifiable {
  id: number = 0;
  name: string = '';
  static count: number = 0;

  constructor(id: number, name: string) {
    super();
    this.id = id;
    this.name = name;
    Item.count += 1;
  }

  label(): string {
    this.logger.log(this.name);
    return helper(this.id);
  }

  static create(id: number): Item {
    return new Item(id, 'item');
  }
}
