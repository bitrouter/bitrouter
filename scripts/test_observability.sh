#!/bin/bash
#
# Local observability testing with Jaeger
# This script sets up a local Jaeger instance and tests the bitrouter
# observability implementation with multi-tenant traces and metrics.

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}BitRouter Observability Testing${NC}"
echo "=================================="

# Check if Docker is available
if ! command -v docker &> /dev/null; then
    echo -e "${RED}Docker is required but not installed. Please install Docker.${NC}"
    exit 1
fi

# Function to cleanup on exit
cleanup() {
    echo -e "\n${YELLOW}Cleaning up...${NC}"
    docker stop jaeger 2>/dev/null || true
    docker rm jaeger 2>/dev/null || true
    pkill -f "bitrouter serve" 2>/dev/null || true
}

# Register cleanup function
trap cleanup EXIT

# Start Jaeger all-in-one (accepts OTLP)
echo -e "\n${GREEN}1. Starting Jaeger...${NC}"
docker run -d --name jaeger \
  -e COLLECTOR_OTLP_ENABLED=true \
  -p 16686:16686 \
  -p 4317:4317 \
  -p 4318:4318 \
  jaegertracing/all-in-one:latest

echo "   Jaeger UI: http://localhost:16686"
echo "   OTLP HTTP: http://localhost:4318"
echo "   OTLP gRPC: http://localhost:4317"

# Wait for Jaeger to be ready
echo -e "\n${YELLOW}Waiting for Jaeger to be ready...${NC}"
sleep 5

# Build bitrouter with observability features
echo -e "\n${GREEN}2. Building bitrouter with observability...${NC}"
cargo build --release --features otel -p bitrouter-observe -p bitrouter

# Configure environment for local OTLP
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_SERVICE_NAME=bitrouter
export RUST_LOG=bitrouter_observe=debug,bitrouter=info

# Create test configuration
echo -e "\n${GREEN}3. Creating test configuration...${NC}"
cat > /tmp/bitrouter-observe-test.yaml << 'EOF'
server:
  listen: "127.0.0.1:8787"
  skip_auth: false

providers:
  mock-anthropic:
    derives: anthropic
    api_base: "http://localhost:9999"
    api_key: "test-key"
    models: [{ id: "claude-3-sonnet" }]
    accounts:
      - { api_key: "key-a", label: "subscription-a" }
      - { api_key: "key-b", label: "subscription-b" }
    account_strategy: balance

plugins:
  bitrouter-observe:
    otel:
      endpoint: "${OTEL_EXPORTER_OTLP_ENDPOINT}"
      sampler: parentbased_always_on
      traces:
        batch:
          max_queue_size: 2048
          flush_ms: 1000
      metrics:
        enabled: true
        export_interval_ms: 10000
        api_key_id_cap: 100
        user_id_cap: 50
EOF

# Start bitrouter
echo -e "\n${GREEN}4. Starting bitrouter...${NC}"
./target/release/bitrouter serve --config /tmp/bitrouter-observe-test.yaml &
BITROUTER_PID=$!

# Wait for bitrouter to start
sleep 3

# Generate test traffic with multiple tenants
echo -e "\n${GREEN}5. Generating multi-tenant test traffic...${NC}"

# Function to send request with specific tenant
send_request() {
    local tenant_id=$1
    local request_num=$2
    local traceparent="00-$(openssl rand -hex 16)-$(openssl rand -hex 8)-01"
    
    curl -s -X POST http://localhost:8787/v1/messages \
      -H "Authorization: Bearer test_key_${tenant_id}" \
      -H "Content-Type: application/json" \
      -H "traceparent: ${traceparent}" \
      -d "{
        \"model\": \"claude-3-sonnet\",
        \"messages\": [{
          \"role\": \"user\",
          \"content\": \"Test request ${request_num} from tenant ${tenant_id}\"
        }]
      }" > /dev/null 2>&1 || true
}

# Send requests from multiple tenants
echo "   Sending 100 requests from 10 different tenants..."
for i in {1..100}; do
    tenant=$((i % 10))
    send_request $tenant $i &
    
    # Rate limit
    if [ $((i % 10)) -eq 0 ]; then
        wait
        echo "   Sent $i requests..."
    fi
done
wait

echo -e "\n${GREEN}6. Running performance benchmarks...${NC}"
cd plugins/bitrouter-observe
cargo bench --features otel

# Display results
echo -e "\n${GREEN}7. Observability Test Results:${NC}"
echo "=================================="
echo "✓ Jaeger UI: http://localhost:16686"
echo "  - Click 'Search' to see traces"
echo "  - Look for 'bitrouter.request' spans"
echo "  - Check tenant attribution in span tags (api_key_id, user_id)"
echo "  - Verify W3C trace context propagation (parent spans)"
echo ""
echo "✓ Multi-account routing:"
echo "  - Check 'account_label' tag alternates between subscription-a and subscription-b"
echo ""
echo "✓ Performance benchmarks completed (see output above)"
echo ""
echo -e "${YELLOW}Press Ctrl+C to stop and cleanup...${NC}"

# Keep running until interrupted
wait $BITROUTER_PID