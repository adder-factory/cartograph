import { Label } from '../src/components/Button';

type Params = { id: string };

const UserPage = ({ id }: Params) => <Label text={id} />;

export default UserPage;
