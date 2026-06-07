import { PageHeader } from '../components/PageHeader';
import { MyRooms } from './MyRooms';

export function CreatorHome() {
  return (
    <div className="page">
      <PageHeader
        title="Creator Studio"
        description="Your channel at a glance. Go live, manage tiers, and track earnings."
      />
      <section className="card">
        <h2 className="section-title">Your channels</h2>
        <MyRooms />
      </section>
    </div>
  );
}
